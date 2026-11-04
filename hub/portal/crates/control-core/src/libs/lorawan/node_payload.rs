//! In-app decoding of NODE LoRaWAN payloads (protobuf), replacing the
//! dependency on the ChirpStack-side JS codec.
//!
//! Decodes the raw application bytes (ChirpStack uplink `data`, base64-decoded)
//! straight from the protobuf schema (`node_proto`, generated from
//! sticker-firmware 435273c6 `app_config.proto`, v2.0.0 sensor channel model):
//!   - fPort 2 → `Telemetry`     → `decode_telemetry`
//!   - fPort 3 → `AlarmReport`   → `decode_alarm_report`
//!
//! Scaling mirrors the reference decoder `app/decoder/ttn.js` (`decodeTelemetry`)
//! exactly so the produced `fields`/`counters`/`events` are identical to what the
//! JS codec emitted. prost handles varint + zigzag (sint32) decoding, so this
//! module only applies the fixed-point scaling and maps to field names.

use std::collections::HashMap;

use serde_json::json;

use control_radio::decode::sensor_types;
use prost::Message;

use super::chirpstack::NodeEvent;
use super::node_proto::{AlarmReport, SensorReading, Telemetry};

/// Decoded telemetry, shaped to match `NodeReading`'s field containers.
#[derive(Debug, Clone, Default)]
pub struct DecodedTelemetry {
    pub fields: HashMap<String, f64>,
    pub counters: HashMap<String, u64>,
    pub events: Vec<NodeEvent>,
    /// The frame's place in a multi-page report, `None` for a complete one.
    pub page: Option<TelemetryPage>,
}

/// One page of a paged fPort-2 report (sticker-firmware #425 paging, PR #480):
/// `Telemetry.page_index` (28) / `page_count` (29). Pages are slices of one
/// snapshot: motherboard groups first, then whole 1-Wire readings, a reading
/// never split. There is no snapshot id; `node_reassembly` correlates pages
/// by arrival order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelemetryPage {
    pub index: u32,
    pub count: u32,
}

impl TelemetryPage {
    /// The page of a frame: `None` when both fields are absent (0, proto3) or
    /// the count is 1, i.e. the frame is the whole report. proto3 omits
    /// `page_index` 0, so the first page carries only its count.
    pub fn of(page_index: u32, page_count: u32) -> Option<Self> {
        (page_count >= 2).then_some(Self {
            index: page_index,
            count: page_count,
        })
    }

    /// Whether the index is one of the report's pages (`index < count`). A page
    /// past its count is malformed and `node_reassembly` never merges it.
    pub fn in_range(self) -> bool {
        self.index < self.count
    }
}

/// fPort-2 "no valid sample" sentinels. An enabled analog sensor is always on
/// the wire (so the configured-sensor list stays stable); a NaN reading arrives
/// as the sentinel and must decode to "field omitted", not a scaled garbage
/// value (e.g. `INT32_MIN / 100 ≈ -21_474_836 °C`). Mirrors `app_compose.c`
/// `TM_S32_NA` / `TM_U32_NA` and `ttn.js` `_TM_S32_NA` / `_TM_U32_NA`; same
/// intent as `node_response.rs`'s `HIST_TEMP_SENTINEL`.
const TM_S32_NA: i32 = i32::MIN; // sint32 fields: temperature, altitude
const TM_U32_NA: u32 = u32::MAX; // uint32 fields: humidity, pressure, illuminance

/// Decode a fPort 2 `Telemetry` frame into scaled fields/counters/events.
/// Scaling per `ttn.js` `decodeTelemetry`: voltage ÷50, temperature ÷100,
/// humidity ÷2, pressure ÷10, altitude ÷10, illuminance ×2.
pub fn decode_telemetry(bytes: &[u8], received_at: &str) -> Result<DecodedTelemetry, String> {
    let t =
        Telemetry::decode(bytes).map_err(|e| format!("Telemetry protobuf decode failed: {e}"))?;
    let mut d = DecodedTelemetry {
        page: TelemetryPage::of(t.page_index, t.page_count),
        ..Default::default()
    };

    let mut event = |ty: &str, extra: serde_json::Value| {
        d.events.push(NodeEvent {
            event_type: ty.to_string(),
            ts: received_at.to_string(),
            extra,
        });
    };

    // --- system ---
    if let Some(v) = t.voltage {
        d.fields.insert("voltage".into(), v as f64 / 50.0);
    }
    if let Some(flags) = t.system_flags {
        if flags & (1 << 0) != 0 {
            event("boot", json!({}));
        }
    }
    // --- internal (SHT4x) ---
    if let Some(v) = t.temperature.filter(|&v| v != TM_S32_NA) {
        d.fields.insert("temperature".into(), v as f64 / 100.0);
    }
    if let Some(v) = t.humidity.filter(|&v| v != TM_U32_NA) {
        d.fields.insert("humidity".into(), v as f64 / 2.0);
    }
    // --- barometer ---
    if let Some(v) = t.pressure.filter(|&v| v != TM_U32_NA) {
        d.fields.insert("pressure".into(), v as f64 / 10.0);
    }
    if let Some(v) = t.altitude.filter(|&v| v != TM_S32_NA) {
        d.fields.insert("altitude".into(), v as f64 / 10.0);
    }
    // --- light ---
    if let Some(v) = t.illuminance.filter(|&v| v != TM_U32_NA) {
        d.fields.insert("illuminance".into(), v as f64 * 2.0);
    }
    // --- accelerometer orientation (surfaced as an event, like the JS codec) ---
    if let Some(v) = t.orientation {
        event("orientation", json!({ "value": v }));
    }
    // --- pir ---
    if let Some(v) = t.motion_count {
        d.counters.insert("motion_count".into(), v as u64);
    }
    // --- hall / input: count is a counter, flags bit2 = active (bits 0/1 retired) ---
    if let Some(v) = t.hall_left_count {
        d.counters.insert("hall_left_count".into(), v as u64);
    }
    if let Some(f) = t.hall_left_flags {
        if f & (1 << 2) != 0 {
            event("hall_active", json!({ "channel": "left", "active": true }));
        }
    }
    if let Some(v) = t.hall_right_count {
        d.counters.insert("hall_right_count".into(), v as u64);
    }
    if let Some(f) = t.hall_right_flags {
        if f & (1 << 2) != 0 {
            event("hall_active", json!({ "channel": "right", "active": true }));
        }
    }
    if let Some(v) = t.input_a_count {
        d.counters.insert("input_a_count".into(), v as u64);
    }
    if let Some(f) = t.input_a_flags {
        if f & (1 << 2) != 0 {
            event("input_active", json!({ "channel": "a", "active": true }));
        }
    }
    if let Some(v) = t.input_b_count {
        d.counters.insert("input_b_count".into(), v as u64);
    }
    if let Some(f) = t.input_b_flags {
        if f & (1 << 2) != 0 {
            event("input_active", json!({ "channel": "b", "active": true }));
        }
    }
    if let Some(v) = t.accel_motion_count {
        d.counters.insert("accel_motion_count".into(), v as u64);
    }

    // --- 1-Wire ROM-bound slots (repeated SensorReading, proto field 27) ---
    //
    // sticker-firmware v2.0.0 (#430, node-firmware-v2.0.0.md §3): a reading is
    // `valid` (bit ch = channel ch has a value) plus the packed values of the set
    // bits in ascending ch, each `round(phys * wire scale)` of that channel in
    // the sensor type registry. Every channel is named by its Hub field (§1.1,
    // `w1_{slot}_{channel}`: w1_2_temperature_aux, w1_3_tilt), the name the v2
    // radio publishes too, so both stacks store the same series. A channel of
    // the type whose bit is clear is a `no_data` event naming the field; a set
    // bit the registry does not know has no name to store under and is logged.
    for sr in &t.w1_sensors {
        w1_slot(sr, &mut d, received_at);
    }

    Ok(d)
}

/// One `SensorReading` (v2.0.0), flattened into `d` (see `decode_telemetry`).
fn w1_slot(sr: &SensorReading, d: &mut DecodedTelemetry, received_at: &str) {
    let slot = u8::try_from(sr.slot).ok();
    let t = slot
        .filter(|s| (1..=sensor_types::W1_SLOTS).contains(s))
        .and_then(|_| u8::try_from(sr.r#type).ok())
        .and_then(sensor_types::sensor_type)
        .filter(|t| t.is_w1());
    let mut push = |ty: &str, extra: serde_json::Value| {
        d.events.push(NodeEvent {
            event_type: ty.to_string(),
            ts: received_at.to_string(),
            extra,
        });
    };
    // `type` is the event's own tag in the flattened envelope: a sensor type
    // stored under that key would overwrite it (§3, amended 2026-10-09).
    push(
        "w1_slot",
        json!({
            "slot": sr.slot,
            "sensor_type": sr.r#type,
            "sensor_type_name": sensor_types::w1_type_label(sr.r#type),
        }),
    );

    let mut values = sr.value.iter();
    for ch in 0..32u8 {
        let set = sr.valid & (1 << ch) != 0;
        let raw = if set { values.next().copied() } else { None };
        let channel = t.and_then(|t| t.channel(ch));
        match (channel, slot, raw) {
            (Some(c), Some(slot), Some(raw)) => {
                let field = sensor_types::hub_field_of(slot, c);
                if c.counter() {
                    d.counters.insert(field, raw.max(0) as u64);
                } else {
                    d.fields.insert(field, f64::from(raw) / c.wire_scale);
                }
            }
            (Some(c), Some(slot), None) => {
                push(
                    "no_data",
                    json!({ "field": sensor_types::hub_field_of(slot, c) }),
                );
            }
            (_, _, Some(raw)) => {
                eprintln!(
                    "[LoRaWAN] 1-Wire slot {} type {}: channel {ch} = {raw} is not in the \
                     sensor type registry; not stored",
                    sr.slot, sr.r#type
                );
            }
            _ => {}
        }
    }
}

/// `AlarmEvent.rule` of the built-in watchdogs (v2.0.0): no data / sensor
/// mismatch, and low battery.
pub const RULE_WATCHDOG: u32 = 0xFF;
pub const RULE_LOW_BATTERY: u32 = 0xFE;

/// Rules are 0..15; the watchdogs above decode as `rule: null`.
const RULE_COUNT: u32 = 16;

/// Decode a fPort 3 `AlarmReport` frame into a list of alarm events. The rule
/// config is not on the wire — each event carries the resulting edge only.
///
/// sticker-firmware v2.0.0 (#430, node-firmware-v2.0.0.md §4): the event keys
/// are the v2 radio's (`decode::node::decode_alarm_batch` flattened by
/// `decode::reading`), so both stacks publish one shape: `slot` label (`mb`,
/// `s1`..), `channel` name (`null` for a mismatch, `device` for 255, `ch{N}`
/// when unknown), `part`, `sensor_type` name (1-Wire slots), `rule` (0..15,
/// `null` for the `0xFF` / `0xFE` watchdogs), `event` (`activate` /
/// `deactivate`), `side` (`low`/`high`/`trigger`/`no_data`/`sensor_mismatch`,
/// not `type`: that key is the event tag), `value` scaled by the channel's wire
/// scale (a mismatch's is the detected type id, `detected_type` its name),
/// `time` and `field`, the Hub field of the channel. The legacy stack adds
/// `edge` (`raise` / `clear`, what the Portal's v1 path reads), `rel_s`,
/// `base_time`, `time_synced` and `value_raw`.
pub fn decode_alarm_report(bytes: &[u8], received_at: &str) -> Result<Vec<NodeEvent>, String> {
    use super::node_alarm::ALARM_TYPE_SENSOR_MISMATCH;
    use super::node_alarm::{alarm_channel, alarm_target, alarm_type_label};
    let r = AlarmReport::decode(bytes)
        .map_err(|e| format!("AlarmReport protobuf decode failed: {e}"))?;
    // time_synced absent = old FW = treat as synced (mirrors ttn.js decodeAlarmBatch).
    let synced = r.time_synced.unwrap_or(true);
    let events = r
        .events
        .iter()
        .map(|e| {
            // Per-event device-clock time = base_time + rel_s, or null when the
            // window opened before the RTC synced (base_time is uptime-relative, so
            // an absolute timestamp would be a bogus ~1970 date). Mirrors ttn.js.
            let time = if synced {
                Some(r.base_time.wrapping_add(e.rel_s))
            } else {
                None
            };
            let kind = u32::try_from(e.r#type).unwrap_or(0);
            let mismatch = kind == ALARM_TYPE_SENSOR_MISMATCH;
            let channel = (!mismatch)
                .then(|| alarm_channel(e.slot, e.sensor_type, e.channel))
                .flatten();
            let value = match (e.value, channel) {
                (Some(v), Some(c)) => scaled(f64::from(v) / c.wire_scale),
                (Some(v), None) => json!(v),
                (None, _) => serde_json::Value::Null,
            };
            // DEACTIVATE = 1; anything else reads as an activation, as ttn.js does.
            let deactivate = e.edge == 1;
            let mut extra = alarm_target(e.slot, e.channel, e.sensor_type, kind);
            let fields = json!({
                "rule": (e.rule < RULE_COUNT).then_some(e.rule),
                "event": if deactivate { "deactivate" } else { "activate" },
                "edge": if deactivate { "clear" } else { "raise" },
                "side": alarm_type_label(kind),
                "rel_s": e.rel_s,
                "base_time": r.base_time,
                "time_synced": synced,
                "time": time, // null when clock unsynced
                "value": value,
                "value_raw": e.value,
            });
            if let serde_json::Value::Object(m) = fields {
                extra.extend(m);
            }
            // Absent, not null, for a mismatch or an unknown channel (as B).
            if let Some(field) = channel
                .zip(u8::try_from(e.slot).ok())
                .map(|(c, s)| sensor_types::hub_field_of(s, c))
            {
                extra.insert("field".into(), json!(field));
            }
            if mismatch {
                if let Some(v) = e.value {
                    extra.insert(
                        "detected_type".into(),
                        json!(u32::try_from(v)
                            .map_or_else(|_| format!("type{v}"), sensor_types::w1_type_label)),
                    );
                }
            }
            NodeEvent {
                event_type: "alarm".to_string(),
                ts: received_at.to_string(),
                extra: serde_json::Value::Object(extra),
            }
        })
        .collect();
    Ok(events)
}

/// A scaled value as JavaScript serializes it (`54`, not `54.0`), so the
/// published JSON matches the v2 radio's (`decode::node::num`).
fn scaled(v: f64) -> serde_json::Value {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 {
        json!(v as i64)
    } else {
        json!(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::lorawan::node_proto::{AlarmEvent, AlarmReport, SensorReading, Telemetry};

    #[test]
    fn telemetry_scaling_matches_ttn_js() {
        let t = Telemetry {
            voltage: Some(150),      // /50 = 3.0 V
            system_flags: Some(0b1), // bit0 boot
            temperature: Some(2300), // /100 = 23.0 °C
            humidity: Some(120),     // /2  = 60.0 %
            pressure: Some(10130),   // /10 = 1013.0 hPa
            altitude: Some(2500),    // /10 = 250.0 m
            illuminance: Some(200),  // *2  = 400 lux
            orientation: Some(3),    // event
            motion_count: Some(7),
            hall_left_count: Some(3),
            hall_left_flags: Some(0b100), // bit2 active
            input_a_count: Some(9),
            ..Default::default()
        };
        let bytes = t.encode_to_vec();
        let d = decode_telemetry(&bytes, "2026-06-18T00:00:00Z").unwrap();

        assert_eq!(d.fields.get("voltage").copied(), Some(3.0));
        assert_eq!(d.fields.get("temperature").copied(), Some(23.0));
        assert_eq!(d.fields.get("humidity").copied(), Some(60.0));
        assert_eq!(d.fields.get("pressure").copied(), Some(1013.0));
        assert_eq!(d.fields.get("altitude").copied(), Some(250.0));
        assert_eq!(d.fields.get("illuminance").copied(), Some(400.0));
        assert_eq!(d.counters.get("motion_count").copied(), Some(7));
        assert_eq!(d.counters.get("hall_left_count").copied(), Some(3));
        assert_eq!(d.counters.get("input_a_count").copied(), Some(9));
        assert!(d.events.iter().any(|e| e.event_type == "boot"));
        assert!(d.events.iter().any(|e| e.event_type == "orientation"));
        assert!(d
            .events
            .iter()
            .any(|e| e.event_type == "hall_active" && e.extra["channel"] == "left"));
    }

    #[test]
    fn telemetry_negative_temperature_zigzag() {
        // sint32 -1550 → -15.5 °C (exercises zigzag via prost)
        let t = Telemetry {
            temperature: Some(-1550),
            ..Default::default()
        };
        let d = decode_telemetry(&t.encode_to_vec(), "t").unwrap();
        assert_eq!(d.fields.get("temperature").copied(), Some(-15.5));
    }

    #[test]
    fn telemetry_sentinels_decode_to_absent_fields() {
        // "No valid sample" sentinels (app_compose.c TM_S32_NA / TM_U32_NA,
        // mirrored by ttn.js → null): an enabled analog sensor is always on the
        // wire, but a NaN reading arrives as the sentinel and must decode to
        // "field omitted", never a scaled garbage value (INT32_MIN/100 ≈
        // -21_474_836 °C — the implausible-temperature bug this guards).
        let t = Telemetry {
            voltage: Some(150),          // no sentinel for voltage → still decodes (3.0 V)
            temperature: Some(i32::MIN), // sint32 sentinel → omit
            humidity: Some(u32::MAX),    // uint32 sentinel → omit
            pressure: Some(u32::MAX),    // uint32 sentinel → omit
            altitude: Some(i32::MIN),    // sint32 sentinel → omit
            illuminance: Some(u32::MAX), // uint32 sentinel → omit
            ..Default::default()
        };
        let d = decode_telemetry(&t.encode_to_vec(), "t").unwrap();

        for k in [
            "temperature",
            "humidity",
            "pressure",
            "altitude",
            "illuminance",
        ] {
            assert!(
                !d.fields.contains_key(k),
                "{k} should be omitted for its sentinel"
            );
        }
        // non-sentinel neighbours still decode
        assert_eq!(d.fields.get("voltage").copied(), Some(3.0));
    }

    /// A v2.0.0 machine-probe reading: temperature, humidity, the tilt state
    /// and accel-x set, temperature-aux clear (no data).
    fn machine_probe(slot: u32) -> SensorReading {
        SensorReading {
            slot,
            r#type: 3,
            valid: 0b110_0011,
            value: vec![-450, 140, 1, 981],
        }
    }

    #[test]
    fn telemetry_w1_slot_by_channel_model() {
        let t = Telemetry {
            w1_sensors: vec![
                machine_probe(2),
                SensorReading {
                    slot: 1,
                    r#type: 2, // dallas
                    valid: 0b1,
                    value: vec![2150],
                },
            ],
            ..Default::default()
        };
        let d = decode_telemetry(&t.encode_to_vec(), "t").unwrap();
        assert_eq!(d.fields.get("w1_2_temperature").copied(), Some(-4.5));
        assert_eq!(d.fields.get("w1_2_humidity").copied(), Some(70.0));
        assert_eq!(d.fields.get("w1_2_tilt").copied(), Some(1.0));
        assert_eq!(d.fields.get("w1_2_accel_x").copied(), Some(9.81));
        assert_eq!(d.fields.get("w1_1_temperature").copied(), Some(21.5));
        assert!(!d.fields.contains_key("w1_2_temperature_aux"));
        let no_data: Vec<_> = d
            .events
            .iter()
            .filter(|e| e.event_type == "no_data")
            .map(|e| e.extra["field"].as_str().unwrap().to_string())
            .collect();
        assert!(no_data.contains(&"w1_2_temperature_aux".to_string()));
        assert!(no_data.contains(&"w1_2_illuminance".to_string()));
        let slots: Vec<_> = d
            .events
            .iter()
            .filter(|e| e.event_type == "w1_slot")
            .map(|e| (e.extra["slot"].clone(), e.extra["sensor_type_name"].clone()))
            .collect();
        assert_eq!(
            slots,
            vec![
                (serde_json::json!(2), serde_json::json!("machine-probe")),
                (serde_json::json!(1), serde_json::json!("dallas")),
            ]
        );
    }

    #[test]
    fn telemetry_w1_unknown_channel_and_empty_slot_store_nothing() {
        let t = Telemetry {
            w1_sensors: vec![
                SensorReading {
                    slot: 3,
                    r#type: 2,
                    valid: 0b101, // dallas has no channel 2
                    value: vec![2000, 99],
                },
                SensorReading {
                    slot: 4,
                    r#type: 0, // empty
                    valid: 0,
                    value: vec![],
                },
            ],
            ..Default::default()
        };
        let d = decode_telemetry(&t.encode_to_vec(), "t").unwrap();
        assert_eq!(d.fields.get("w1_3_temperature").copied(), Some(20.0));
        assert_eq!(d.fields.len(), 1);
        assert_eq!(
            d.events
                .iter()
                .filter(|e| e.event_type == "w1_slot")
                .count(),
            2
        );
        assert!(!d.events.iter().any(|e| e.event_type == "no_data"));
    }

    #[test]
    fn alarm_report_decodes_events() {
        let r = AlarmReport {
            base_time: 1_780_000_000,
            total: 1,
            time_synced: None, // absent → treated as synced
            events: vec![AlarmEvent {
                slot: 0,
                edge: 0,
                r#type: 2, // TYPE_HIGH
                rel_s: 5,
                value: Some(5500),
                rule: 3,
                channel: 0, // board temperature, wire scale 100
                sensor_type: None,
            }],
            ..Default::default()
        };
        let evs = decode_alarm_report(&r.encode_to_vec(), "t").unwrap();
        assert_eq!(evs.len(), 1);
        let e = &evs[0].extra;
        assert_eq!(evs[0].event_type, "alarm");
        assert_eq!(e["slot"], "mb");
        assert_eq!(e["rule"], 3);
        assert_eq!(e["channel"], "temperature");
        assert_eq!(e["field"], "temperature");
        assert_eq!(e["value"], 55);
        assert_eq!(e["value_raw"], 5500);
        assert_eq!(e["side"], "high");
        assert_eq!(e["event"], "activate");
        assert_eq!(e["edge"], "raise");
        assert!(e.get("sensor_type").is_none(), "slot 0 names no type");
        assert_eq!(e["time"], 1_780_000_005u32); // base_time + rel_s (synced)
    }

    /// The published event is still an `alarm` after the JSON round trip: no
    /// key of the decoded map collides with the envelope's `type` tag.
    #[test]
    fn alarm_and_w1_slot_events_keep_their_tag_on_the_wire() {
        let r = AlarmReport {
            base_time: 1,
            total: 1,
            events: vec![AlarmEvent {
                slot: 1,
                r#type: 2,
                channel: 0,
                sensor_type: Some(3),
                ..Default::default()
            }],
            ..Default::default()
        };
        let t = Telemetry {
            w1_sensors: vec![machine_probe(2)],
            ..Default::default()
        };
        let mut events = decode_alarm_report(&r.encode_to_vec(), "t").unwrap();
        events.extend(decode_telemetry(&t.encode_to_vec(), "t").unwrap().events);
        for ev in &events {
            let json = serde_json::to_string(ev).unwrap();
            assert!(
                json.contains(&format!("\"type\":\"{}\"", ev.event_type)),
                "{json}"
            );
            let back: NodeEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(back.event_type, ev.event_type);
        }
        let slot = events
            .iter()
            .find(|e| e.event_type == "w1_slot")
            .expect("w1_slot");
        assert_eq!(slot.extra["sensor_type"], 3);
        assert_eq!(slot.extra["sensor_type_name"], "machine-probe");
        assert!(slot.extra.get("type").is_none());
    }

    /// Stack A publishes the keys stack B does for the same bytes; A only adds
    /// `edge`, `rel_s`, `base_time`, `time_synced` and `value_raw`.
    #[test]
    fn alarm_report_matches_the_v2_radio() {
        use control_radio::decode::{node, reading, Decoded};
        let ev = |slot, r#type, rule, channel, sensor_type, value, edge| AlarmEvent {
            slot,
            edge,
            r#type,
            rel_s: 3,
            value,
            rule,
            channel,
            sensor_type,
        };
        let r = AlarmReport {
            base_time: 1_780_000_000,
            total: 5,
            time_synced: Some(true),
            events: vec![
                ev(0, 2, 3, 0, None, Some(5550), 0),
                ev(1, 1, 0, 2, Some(3), Some(-250), 1),
                ev(2, 5, RULE_WATCHDOG, 0, Some(3), Some(2), 0),
                ev(3, 4, RULE_WATCHDOG, 255, Some(3), None, 0),
                ev(3, 4, RULE_WATCHDOG, 1, Some(3), None, 1),
                ev(0, 4, RULE_LOW_BATTERY, 9, None, Some(120), 0),
            ],
            ..Default::default()
        };
        let bytes = r.encode_to_vec();
        let a = decode_alarm_report(&bytes, "t").unwrap();
        let decoded = Decoded {
            data: node::decode_alarm_batch(&bytes),
            warnings: vec![],
            errors: vec![],
        };
        let (_, _, b) = reading::from_decoded(&decoded, "t");
        let b: Vec<_> = b.into_iter().filter(|e| e.event_type == "alarm").collect();
        assert_eq!(a.len(), b.len());
        const A_ONLY: &[&str] = &["edge", "rel_s", "base_time", "time_synced", "value_raw"];
        for (a, b) in a.iter().zip(&b) {
            let a = a.extra.as_object().unwrap();
            let mut a_keys: Vec<_> = a.keys().filter(|k| !A_ONLY.contains(&k.as_str())).collect();
            let mut b_keys: Vec<_> = b.extra.keys().collect();
            a_keys.sort();
            b_keys.sort();
            assert_eq!(a_keys, b_keys, "{a:?} vs {:?}", b.extra);
            for (k, bv) in &b.extra {
                let av = &a[k];
                match (av.as_f64(), bv.as_f64()) {
                    (Some(x), Some(y)) => assert_eq!(x, y, "{k}"),
                    _ => assert_eq!(av, bv, "{k}"),
                }
            }
        }
    }

    #[test]
    fn alarm_report_w1_channel_and_sensor_mismatch() {
        let r = AlarmReport {
            base_time: 100,
            total: 2,
            time_synced: Some(false),
            events: vec![
                AlarmEvent {
                    slot: 1,
                    edge: 0,
                    r#type: 1, // LOW
                    rel_s: 0,
                    value: Some(-250),
                    rule: 0,
                    channel: 2, // temperature-aux
                    sensor_type: Some(3),
                },
                AlarmEvent {
                    slot: 2,
                    edge: 0,
                    r#type: 5, // SENSOR_MISMATCH
                    rel_s: 1,
                    value: Some(2), // detected dallas
                    rule: RULE_WATCHDOG,
                    channel: 0,
                    sensor_type: Some(3), // expected machine-probe
                },
            ],
            ..Default::default()
        };
        let evs = decode_alarm_report(&r.encode_to_vec(), "t").unwrap();
        let a = &evs[0].extra;
        assert_eq!(a["field"], "w1_1_temperature_aux");
        assert_eq!(a["channel"], "temperature-aux");
        assert_eq!(a["value"], -2.5);
        assert_eq!(a["sensor_type"], "machine-probe");
        assert!(a["time"].is_null());
        let m = &evs[1].extra;
        assert_eq!(m["side"], "sensor_mismatch");
        assert!(m["channel"].is_null());
        assert!(m.get("field").is_none());
        assert_eq!(m["value"], 2); // raw: the detected type id
        assert_eq!(m["detected_type"], "dallas");
        assert_eq!(m["sensor_type"], "machine-probe");
        assert!(m["rule"].is_null(), "a watchdog has no rule");
    }

    #[test]
    fn alarm_report_whole_1wire_device_no_data() {
        let r = AlarmReport {
            base_time: 100,
            total: 1,
            events: vec![AlarmEvent {
                slot: 3,
                r#type: 4, // NO_DATA
                rule: RULE_WATCHDOG,
                channel: 255, // the whole device stopped answering
                sensor_type: Some(3),
                ..Default::default()
            }],
            ..Default::default()
        };
        let evs = decode_alarm_report(&r.encode_to_vec(), "t").unwrap();
        let e = &evs[0].extra;
        assert_eq!(e["channel"], "device");
        assert!(e["rule"].is_null());
        assert!(e["field"].is_null());
    }
}
