//! The `"node"` decoder — `node-firmware-v1.4.0.md` §3-§4, proven by parity against
//! `app/decoder/ttn.js@v1.4.0`, and the v2.0.0 `system_flags` alarm byte (same doc, amendment
//! 2026-09-25). *Amended 2026-10-09 (`node-firmware-v2.0.0.md` §3-§4, ttn.js at 435273c6):*
//! a `SensorReading` is `valid` + packed `values` read through the sensor type registry, and an
//! alarm names its slot, channel and rule.
//!
//! Output is a `serde_json` map rather than a struct, because the parity contract is
//! field-for-field identity with the reference decoder and that includes **presence**: an
//! absent sensor omits its key, while an enabled sensor with no valid sample emits the key
//! with an explicit `null`. A struct of `Option`s cannot express that difference without
//! extra machinery, and the difference is the point — it is how a dashboard distinguishes
//! "not fitted" from "fitted and broken".

use serde_json::{Map, Value};

use crate::decode::pb::{read_tag, read_varint, skip, zigzag};
use crate::decode::sensor_types::{self, MOTHERBOARD};

/// "Value not available" sentinels, mirroring `app_compose.c`. An enabled analog sensor is
/// always on the wire so the configured-sensor list stays stable; a NaN reading arrives as
/// the sentinel and decodes to null.
const S32_NA: i32 = i32::MIN;
const U32_NA: u32 = u32::MAX;

/// The alarm half of the firmware's device-status word (ttn.js `_DEVICE_STATUS`, bits below 8),
/// the part telemetry carries in `system_flags`.
const DEVICE_STATUS_ALARMS: &[(u32, &str)] = &[
    (1 << 0, "alarm_any"),
    (1 << 1, "alarm_threshold"),
    (1 << 2, "alarm_state"),
    (1 << 3, "alarm_rate"),
    (1 << 4, "alarm_no_data"),
    (1 << 5, "alarm_low_battery"),
    // v2.0.0 (#430): a 1-Wire slot holds a sensor of another type than configured.
    (1 << 6, "alarm_sensor_mismatch"),
];

const ALARM_EDGES: &[&str] = &["activate", "deactivate"];
/// `AlarmEvent.type` / `AlarmStatus.type`; v2.0.0 adds `sensor_mismatch` (5).
pub const ALARM_TYPES: &[&str] = &[
    "none",
    "low",
    "high",
    "trigger",
    "no_data",
    "sensor_mismatch",
];
const ALARM_TYPE_NO_DATA: u32 = 4;
const ALARM_TYPE_SENSOR_MISMATCH: u32 = 5;
/// `AlarmEvent.channel` / `AlarmStatus.channel` of a whole 1-Wire device's no-data (eff819ed).
const ALARM_CH_DEVICE: u32 = 255;
/// Rules are 0..15; `AlarmEvent.rule` 0xFF (no-data / mismatch watchdog) and 0xFE (low battery)
/// decode as `rule: null`.
const ALARM_RULE_COUNT: u32 = 16;

/// Emit a number the way JavaScript would serialize it: an integral value becomes an integer,
/// not `54.0`. Without this every scaled field that lands on a whole number differs from the
/// reference in its JSON representation, and parity fails on formatting rather than on
/// meaning.
fn num(v: f64) -> Value {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 {
        Value::from(v as i64)
    } else {
        Value::from(v)
    }
}

fn scaled(raw: i64, divisor: f64) -> Value {
    num(raw as f64 / divisor)
}

/// fPort 2 — flat `Telemetry`.
///
/// *Amended 2026-10-14 (v2.0.0 only):* the retired fields 10..17 (the pre-1.5 flat 1-Wire
/// probes, `reserved` in the 2.0.0 proto) are no longer decoded, and 30/31 are an analog GP
/// input's voltage (mV on the wire, V here, `ttn.js` 408eeaf).
pub fn decode_telemetry(bytes: &[u8]) -> Map<String, Value> {
    let mut d = Map::new();
    let mut pos = 0usize;

    while pos < bytes.len() {
        let tag = read_tag(bytes, pos);
        pos = tag.next;

        // Field 27 is the repeated ROM-bound slot reading, the one length-delimited field.
        if tag.field == 27 && tag.wire == 2 {
            let sl = read_varint(bytes, pos);
            pos = sl.next;
            let end = (pos + sl.value as usize).min(bytes.len());
            let reading = decode_sensor_reading(bytes, pos, end);
            d.entry("w1_sensors")
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Some(Value::Array(a)) = d.get_mut("w1_sensors") {
                a.push(Value::Object(reading));
            }
            pos += sl.value as usize;
            continue;
        }

        if tag.wire != 0 {
            // Forward-compatible: skip an unknown non-varint field, stop on a wire type the
            // reference does not skip either.
            match skip(bytes, tag.wire, pos) {
                Some(next) => {
                    pos = next;
                    continue;
                }
                None => break,
            }
        }

        let v = read_varint(bytes, pos);
        pos = v.next;
        let raw = v.value;

        match tag.field {
            // system
            1 => {
                // 0 is the pre-sample sentinel, not 0 V.
                d.insert(
                    "voltage".into(),
                    if raw == 0 {
                        Value::Null
                    } else {
                        scaled(raw as i64, 50.0)
                    },
                );
            }
            // system_flags: bit 0 boot, bits 1..8 the device-status alarm byte (sticker-firmware
            // #409, ttn.js v2.0.0): the Node's own alarm state beside the reading.
            2 => {
                d.insert("boot".into(), Value::Bool(raw & 1 != 0));
                let status = (raw >> 1) & 0xff;
                d.insert("alarm_status".into(), Value::from(status));
                d.insert(
                    "alarm_status_flags".into(),
                    Value::Array(
                        DEVICE_STATUS_ALARMS
                            .iter()
                            .filter(|(bit, _)| status & *bit != 0)
                            .map(|(_, name)| Value::from(*name))
                            .collect(),
                    ),
                );
            }
            // internal SHT4x
            3 => {
                let t = zigzag(raw);
                d.insert(
                    "temperature".into(),
                    if t == S32_NA {
                        Value::Null
                    } else {
                        scaled(t as i64, 100.0)
                    },
                );
            }
            4 => {
                d.insert(
                    "humidity".into(),
                    if raw == U32_NA {
                        Value::Null
                    } else {
                        scaled(raw as i64, 2.0)
                    },
                );
            }
            // barometer
            5 => {
                d.insert(
                    "pressure".into(),
                    if raw == U32_NA {
                        Value::Null
                    } else {
                        scaled(raw as i64, 10.0)
                    },
                );
            }
            6 => {
                let a = zigzag(raw);
                d.insert(
                    "altitude".into(),
                    if a == S32_NA {
                        Value::Null
                    } else {
                        scaled(a as i64, 10.0)
                    },
                );
            }
            // light
            7 => {
                d.insert(
                    "illuminance".into(),
                    if raw == U32_NA {
                        Value::Null
                    } else {
                        num(raw as f64 * 2.0)
                    },
                );
            }
            8 => {
                d.insert(
                    "orientation".into(),
                    if raw == U32_NA {
                        Value::Null
                    } else {
                        Value::from(raw)
                    },
                );
            }
            9 => {
                d.insert("motion_count".into(), Value::from(raw));
            }
            // Counters and their flag words. Bits 0/1 were notify flags, retired with
            // dynamic alarms; active is bit 2.
            18 => {
                d.insert("hall_left_count".into(), Value::from(raw));
            }
            19 => {
                d.insert(
                    "hall_left_is_active".into(),
                    Value::Bool(raw & (1 << 2) != 0),
                );
            }
            20 => {
                d.insert("hall_right_count".into(), Value::from(raw));
            }
            21 => {
                d.insert(
                    "hall_right_is_active".into(),
                    Value::Bool(raw & (1 << 2) != 0),
                );
            }
            22 => {
                d.insert("input_a_count".into(), Value::from(raw));
            }
            23 => {
                d.insert("input_a_is_active".into(), Value::Bool(raw & (1 << 2) != 0));
            }
            24 => {
                d.insert("input_b_count".into(), Value::from(raw));
            }
            25 => {
                d.insert("input_b_is_active".into(), Value::Bool(raw & (1 << 2) != 0));
            }
            26 => {
                d.insert("accel_motion_count".into(), Value::from(raw));
            }
            // #425/#480 paging, as Response / AlarmReport.
            28 => {
                d.insert("page_index".into(), Value::from(raw));
            }
            29 => {
                d.insert("page_count".into(), Value::from(raw));
            }
            // v2.0.0 (#396): an analog GP input's voltage, mV.
            // sticker-firmware PR #507 sends U32_NA for an enabled input without a sample.
            30 => {
                d.insert("input_a_voltage".into(), millivolts(raw));
            }
            31 => {
                d.insert("input_b_voltage".into(), millivolts(raw));
            }
            // Unknown field: ignore, forward-compatibly.
            _ => {}
        }
    }
    apply_telemetry_pages(&mut d);
    d
}

/// A report that does not fit one frame comes as pages (`page_index` 0..`page_count`-1 of one
/// snapshot, sticker-firmware #480), each decoded on its own: `pages` is `"i/N"`, 1-based, and
/// the proto3-absent first index is 0 — `ttn.js` `_applyTelemetryPages`. A 1-Wire reading is
/// never split, so a reading on a page is complete.
fn apply_telemetry_pages(d: &mut Map<String, Value>) {
    let count = d.get("page_count").and_then(Value::as_u64).unwrap_or(0);
    if count > 1 {
        let index = d.get("page_index").and_then(Value::as_u64).unwrap_or(0);
        d.insert("page_index".into(), Value::from(index));
        d.insert(
            "pages".into(),
            Value::from(format!("{}/{count}", index + 1)),
        );
    }
}

/// One `SensorReading` (Telemetry field 27, v2.0.0): slot 1, type 2, `valid` 11 (bit `ch` =
/// channel `ch` has a value), `values` 12 (packed sint32, one per set bit in ascending `ch`,
/// each the physical value times the channel's wire scale). Every channel of the type is in
/// `values` (null when its bit is clear) with its unit in `units`; a set bit the registry does
/// not know comes out raw as `ch{N}`, as `ttn.js` has it.
fn decode_sensor_reading(bytes: &[u8], start: usize, end: usize) -> Map<String, Value> {
    let mut sr = Map::new();
    let mut valid: u32 = 0;
    let mut raw: Vec<i32> = Vec::new();
    let mut type_id: Option<u32> = None;
    let mut pos = start;

    while pos < end && pos < bytes.len() {
        let tag = read_tag(bytes, pos);
        pos = tag.next;
        if tag.field == 12 && tag.wire == 2 {
            let len = read_varint(bytes, pos);
            pos = len.next;
            let packed_end = pos + len.value as usize;
            while pos < packed_end && pos < bytes.len() {
                let v = read_varint(bytes, pos);
                pos = v.next;
                raw.push(zigzag(v.value));
            }
            continue;
        }
        if tag.wire != 0 {
            match skip(bytes, tag.wire, pos) {
                Some(next) => {
                    pos = next;
                    continue;
                }
                None => break,
            }
        }
        let v = read_varint(bytes, pos);
        pos = v.next;
        match tag.field {
            1 => {
                sr.insert("slot".into(), Value::from(v.value));
            }
            2 => {
                type_id = Some(v.value);
                sr.insert("type".into(), Value::from(v.value));
                let name = match sensor_types::w1_type_label(v.value) {
                    l if l.starts_with("type") => "unknown".to_string(),
                    l => l,
                };
                sr.insert("type_name".into(), Value::from(name));
            }
            11 => valid = v.value,
            12 => raw.push(zigzag(v.value)),
            _ => {}
        }
    }

    let t = type_id
        .and_then(|id| u8::try_from(id).ok())
        .and_then(sensor_types::sensor_type);
    let mut values = Map::new();
    let mut units = Map::new();
    let mut next = raw.into_iter();
    for ch in 0..32u8 {
        let has = valid >> ch & 1 == 1;
        let r = if has { next.next() } else { None };
        let Some(c) = t.and_then(|t| t.channel(ch)) else {
            if let Some(r) = r {
                values.insert(format!("ch{ch}"), Value::from(r));
            }
            continue;
        };
        if c.retired() {
            continue;
        }
        values.insert(
            c.name.to_string(),
            r.map_or(Value::Null, |r| scaled(i64::from(r), c.wire_scale)),
        );
        units.insert(c.name.to_string(), Value::from(c.unit));
    }
    sr.insert("values".into(), Value::Object(values));
    sr.insert("units".into(), Value::Object(units));
    sr
}

/// The channel an alarm on `(slot, channel)` is about: the motherboard type on slot 0, else
/// the type the event names (any registry type, as `ttn.js`'s `_alarmChannel`).
fn alarm_channel(
    slot: u32,
    sensor_type: Option<u32>,
    ch: u32,
) -> Option<&'static sensor_types::Channel> {
    let id = if slot == 0 {
        u32::from(MOTHERBOARD)
    } else {
        sensor_type?
    };
    let t = sensor_types::sensor_type(u8::try_from(id).ok()?)?;
    t.channel(u8::try_from(ch).ok()?)
}

/// The target of an alarm — `slot` (`mb`, `s1`..), `channel` (null for a sensor mismatch,
/// which concerns the whole slot; `device` for the no-data of a whole 1-Wire device, channel
/// 255; `ch{N}` when unknown), `part` for a 1-Wire no-data on a channel (the chip that stopped)
/// and, for a 1-Wire slot event that carries it, `sensor_type` — as `ttn.js`'s `_alarmTarget`
/// (eff819ed). Shared by fPort 3 events and
/// `Info.active_alarms`.
pub(crate) fn alarm_target(
    slot: u32,
    ch: u32,
    sensor_type: Option<u32>,
    kind: u32,
) -> Map<String, Value> {
    let mut o = Map::new();
    o.insert("slot".into(), Value::from(sensor_types::slot_label(slot)));
    let c = alarm_channel(slot, sensor_type, ch);
    o.insert(
        "channel".into(),
        if kind == ALARM_TYPE_SENSOR_MISMATCH {
            Value::Null
        } else if ch == ALARM_CH_DEVICE {
            Value::from("device")
        } else {
            Value::from(
                c.map(|c| c.name.to_string())
                    .unwrap_or_else(|| format!("ch{ch}")),
            )
        },
    );
    // A 1-Wire no-data on a channel stands for every channel of its chip.
    if let (true, Some(part)) = (kind == ALARM_TYPE_NO_DATA, c.and_then(|c| c.part)) {
        o.insert("part".into(), Value::from(part));
    }
    if let (Some(t), true) = (sensor_type, slot != 0) {
        o.insert(
            "sensor_type".into(),
            Value::from(sensor_types::w1_type_label(t)),
        );
    }
    o
}

/// `AlarmEvent.type` / `AlarmStatus.type` by name (`none` past the table, as `ttn.js`).
pub(crate) fn alarm_type_label(kind: u32) -> &'static str {
    ALARM_TYPES.get(kind as usize).copied().unwrap_or("none")
}

struct AlarmEvent {
    rule: u32,
    slot: u32,
    channel: u32,
    sensor_type: Option<u32>,
    edge: u32,
    kind: u32,
    rel_s: u32,
    value: Option<i32>,
}

fn decode_alarm_event(bytes: &[u8], start: usize, end: usize) -> AlarmEvent {
    let mut ev = AlarmEvent {
        rule: 0,
        slot: 0,
        channel: 0,
        sensor_type: None,
        edge: 0,
        kind: 0,
        rel_s: 0,
        value: None,
    };
    let mut pos = start;
    while pos < end && pos < bytes.len() {
        let tag = read_tag(bytes, pos);
        pos = tag.next;
        if tag.wire == 0 {
            let v = read_varint(bytes, pos);
            pos = v.next;
            match tag.field {
                1 => ev.slot = v.value,
                2 => ev.edge = v.value,
                4 => ev.rel_s = v.value,
                5 => ev.value = Some(zigzag(v.value)),
                7 => ev.rule = v.value,
                9 => ev.kind = v.value,
                10 => ev.channel = v.value,
                // 1-Wire slot events only.
                11 => ev.sensor_type = Some(v.value),
                _ => {}
            }
        } else if tag.wire == 2 {
            let l = read_varint(bytes, pos);
            pos = l.next + l.value as usize;
        } else {
            break;
        }
    }
    ev
}

/// fPort 3 — the `AlarmReport` batch.
pub fn decode_alarm_batch(bytes: &[u8]) -> Map<String, Value> {
    let mut base_time: u32 = 0;
    let mut total: u32 = 0;
    // Field 4 absent means old firmware, which is treated as synced so timestamps are still
    // emitted.
    let mut time_synced = true;
    // #425: a batch split across frames numbers its pages like a `Response` does.
    let (mut page_index, mut page_count) = (0u32, 0u32);
    let mut alarms: Vec<Map<String, Value>> = Vec::new();
    let mut rels: Vec<u32> = Vec::new();

    let mut pos = 0usize;
    while pos < bytes.len() {
        let tag = read_tag(bytes, pos);
        pos = tag.next;
        if tag.wire == 0 {
            let v = read_varint(bytes, pos);
            pos = v.next;
            match tag.field {
                1 => base_time = v.value,
                2 => total = v.value,
                4 => time_synced = v.value != 0,
                5 => page_index = v.value,
                6 => page_count = v.value,
                _ => {}
            }
        } else if tag.wire == 2 {
            let len = read_varint(bytes, pos);
            pos = len.next;
            let end = pos + len.value as usize;
            if tag.field == 3 {
                let ev = decode_alarm_event(bytes, pos, end.min(bytes.len()));
                let mismatch = ev.kind == ALARM_TYPE_SENSOR_MISMATCH;
                let c = alarm_channel(ev.slot, ev.sensor_type, ev.channel);
                let mut a = alarm_target(ev.slot, ev.channel, ev.sensor_type, ev.kind);
                // Rules 0..15; the watchdogs (no-data / mismatch 0xFF, battery 0xFE) have
                // none: type and channel say which one fired.
                a.insert(
                    "rule".into(),
                    if ev.rule < ALARM_RULE_COUNT {
                        Value::from(ev.rule)
                    } else {
                        Value::Null
                    },
                );
                a.insert(
                    "event".into(),
                    Value::from(
                        ALARM_EDGES
                            .get(ev.edge as usize)
                            .copied()
                            .unwrap_or("activate"),
                    ),
                );
                a.insert("type".into(), Value::from(alarm_type_label(ev.kind)));
                // The channel value times its wire scale; a mismatch carries the detected
                // type id instead, and an unknown channel stays raw.
                a.insert(
                    "value".into(),
                    match (ev.value, c) {
                        (None, _) => Value::Null,
                        (Some(raw), Some(c)) if !mismatch => scaled(i64::from(raw), c.wire_scale),
                        (Some(raw), _) => Value::from(raw),
                    },
                );
                a.insert("time".into(), Value::from(0));
                if let (true, Some(raw)) = (mismatch, ev.value) {
                    a.insert(
                        "detected_type".into(),
                        Value::from(match u32::try_from(raw) {
                            Ok(id) => sensor_types::w1_type_label(id),
                            Err(_) => format!("type{raw}"),
                        }),
                    );
                }
                alarms.push(a);
                rels.push(ev.rel_s);
            }
            pos = end;
        } else {
            break;
        }
    }

    // base_time precedes the events on the wire, but times are resolved after the loop so
    // field order cannot bite. A null time means the clock had not synced at window start —
    // base_time is then uptime-relative and an absolute timestamp would be a lie.
    for (a, rel) in alarms.iter_mut().zip(&rels) {
        a.insert(
            "time".into(),
            if time_synced {
                Value::from(base_time.wrapping_add(*rel))
            } else {
                Value::Null
            },
        );
    }

    let mut out = Map::new();
    out.insert("base_time".into(), Value::from(base_time));
    out.insert("total".into(), Value::from(total));
    out.insert("time_synced".into(), Value::Bool(time_synced));
    // `total` counts every alarm of the window. A batch split across frames (#425) carries a
    // disjoint part of it per page, labelled like `ttn.js` does; only an unpaged batch with
    // fewer events than `total` has dropped some.
    if page_count >= 2 {
        out.insert("page_index".into(), Value::from(page_index));
        out.insert("page_count".into(), Value::from(page_count));
        out.insert(
            "pages".into(),
            Value::from(format!("{}/{}", page_index + 1, page_count)),
        );
    } else {
        out.insert(
            "truncated".into(),
            Value::Bool(alarms.len() < total as usize),
        );
    }
    out.insert(
        "alarms".into(),
        Value::Array(alarms.into_iter().map(Value::Object).collect()),
    );
    out
}

/// An analog input's voltage in volts, or null for the "not available" sentinel.
fn millivolts(raw: u32) -> Value {
    if raw == U32_NA {
        Value::Null
    } else {
        scaled(raw as i64, 1000.0)
    }
}

#[cfg(test)]
mod alarm_status_tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// #90: bits 1..8 of `system_flags` are the Node's alarm byte (sticker-firmware #409),
    /// captured on 0413 with alarms active and idle.
    #[test]
    fn system_flags_carry_the_alarm_byte_beside_boot() {
        // The leading 01 is the application protocol version, not part of the Telemetry.
        let active = decode_telemetry(&unhex("0108a601100618c6292074")[1..]);
        assert_eq!(active["boot"], false);
        assert_eq!(active["alarm_status"], 3);
        assert_eq!(
            active["alarm_status_flags"],
            serde_json::json!(["alarm_any", "alarm_threshold"])
        );

        let idle = decode_telemetry(&unhex("0108a601100018922a2071")[1..]);
        assert_eq!(idle["alarm_status"], 0);
        assert_eq!(idle["alarm_status_flags"], serde_json::json!([]));
    }
}

#[cfg(test)]
mod paging_tests {
    use super::*;

    /// A batch split across frames (#425): its page numbers, and no `truncated` — each page
    /// carries only part of `total` by design.
    #[test]
    fn a_paged_alarm_batch_is_labelled_and_not_called_truncated() {
        // base_time 100, total 3, one event, page_index 1, page_count 3.
        let paged = decode_alarm_batch(&[
            0x08, 0x64, 0x10, 0x03, 0x1a, 0x02, 0x38, 0x01, 0x28, 0x01, 0x30, 0x03,
        ]);
        assert_eq!(paged["pages"], "2/3");
        assert_eq!(paged["page_index"], 1);
        assert_eq!(paged["page_count"], 3);
        assert!(paged.get("truncated").is_none());

        // The same frame unpaged has dropped two of its three alarms.
        let unpaged = decode_alarm_batch(&[0x08, 0x64, 0x10, 0x03, 0x1a, 0x02, 0x38, 0x01]);
        assert_eq!(unpaged["truncated"], true);
        assert!(unpaged.get("pages").is_none());
    }
}

#[cfg(test)]
mod sentinel_tests {
    use super::*;

    /// sticker-firmware PR #507: an enabled sensor without a valid sample sends the
    /// sentinel, which decodes to null; orientation and the input voltages are new to it.
    #[test]
    fn na_sentinels_decode_to_null() {
        // temperature (3) = INT32_MIN, humidity (4), orientation (8), input_a_voltage (30)
        // and input_b_voltage (31) = UINT32_MAX, all as varint ffffffff0f.
        let na = [0xff, 0xff, 0xff, 0xff, 0x0f];
        let mut frame = Vec::new();
        for tag in [0x18u8, 0x20, 0x40] {
            frame.push(tag);
            frame.extend_from_slice(&na);
        }
        for tag in [[0xf0u8, 0x01], [0xf8, 0x01]] {
            frame.extend_from_slice(&tag);
            frame.extend_from_slice(&na);
        }
        let d = decode_telemetry(&frame);
        for key in [
            "temperature",
            "humidity",
            "orientation",
            "input_a_voltage",
            "input_b_voltage",
        ] {
            assert_eq!(d[key], Value::Null, "{key}");
        }
    }

    #[test]
    fn real_input_voltage_and_orientation_still_decode() {
        // orientation 2, input_a_voltage 3300 mV.
        let d = decode_telemetry(&[0x40, 0x02, 0xf0, 0x01, 0xe4, 0x19]);
        assert_eq!(d["orientation"], 2);
        assert_eq!(d["input_a_voltage"], 3.3);
    }
}
