//! In-app decoding of STICKER LoRaWAN payloads (protobuf), replacing the
//! dependency on the ChirpStack-side JS codec.
//!
//! Decodes the raw application bytes (ChirpStack uplink `data`, base64-decoded)
//! straight from the protobuf schema (`sticker_proto`, generated from
//! sticker-firmware v1.4.0 `app_config.proto`):
//!   - fPort 2 → `Telemetry`     → `decode_telemetry`
//!   - fPort 3 → `AlarmReport`   → `decode_alarm_report`
//!
//! Scaling mirrors the reference decoder `app/decoder/ttn.js` (`decodeTelemetry`)
//! exactly so the produced `fields`/`counters`/`events` are identical to what the
//! JS codec emitted. prost handles varint + zigzag (sint32) decoding, so this
//! module only applies the fixed-point scaling and maps to field names.

use std::collections::HashMap;

use serde_json::json;

use super::chirpstack::StickerEvent;
use super::sticker_proto::{AlarmReport, Telemetry};
use prost::Message;

/// Decoded telemetry, shaped to match `StickerReading`'s field containers.
#[derive(Debug, Clone, Default)]
pub struct DecodedTelemetry {
    pub fields: HashMap<String, f64>,
    pub counters: HashMap<String, u64>,
    pub events: Vec<StickerEvent>,
}

/// 1-Wire slot type (mirrors `enum app_w1_slot_type` in app_w1_slots.h).
const W1_TYPE_MACHINE_PROBE: u32 = 2;

/// Decode a fPort 2 `Telemetry` frame into scaled fields/counters/events.
/// Scaling per `ttn.js` `decodeTelemetry`: voltage ÷50, temperature ÷100,
/// humidity ÷2, pressure ÷10, altitude ÷10, illuminance ×2.
pub fn decode_telemetry(bytes: &[u8], received_at: &str) -> Result<DecodedTelemetry, String> {
    let t = Telemetry::decode(bytes).map_err(|e| format!("Telemetry protobuf decode failed: {e}"))?;
    let mut d = DecodedTelemetry::default();

    let mut event = |ty: &str, extra: serde_json::Value| {
        d.events.push(StickerEvent {
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
    if let Some(v) = t.temperature {
        d.fields.insert("temperature".into(), v as f64 / 100.0);
    }
    if let Some(v) = t.humidity {
        d.fields.insert("humidity".into(), v as f64 / 2.0);
    }
    // --- barometer ---
    if let Some(v) = t.pressure {
        d.fields.insert("pressure".into(), v as f64 / 10.0);
    }
    if let Some(v) = t.altitude {
        d.fields.insert("altitude".into(), v as f64 / 10.0);
    }
    // --- light ---
    if let Some(v) = t.illuminance {
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
    // MAPPING CONVENTION (TBD — confirm with firmware/product):
    //   machine-probe slot N → machine_probe_temperature_{N} / machine_probe_humidity_{N}
    //                          (+ tilt event), other quantities as ext_{quantity}_{N}
    //   plain (dallas) slot N → ext_temperature_{N}
    //
    // ttn.js leaves w1_sensors as a nested array and the legacy Rust path only
    // knew the retired flat fields 10-17, so there is no prior host-side flat
    // mapping to match — this is a new, documented convention. The viewer field
    // registry currently names only _1/_2; higher slots are still recorded
    // generically (just without a pretty registry label).
    for sr in &t.w1_sensors {
        let n = sr.slot;
        let probe = sr.r#type == W1_TYPE_MACHINE_PROBE;
        let temp_key = if probe {
            format!("machine_probe_temperature_{n}")
        } else {
            format!("ext_temperature_{n}")
        };
        if let Some(v) = sr.temperature {
            d.fields.insert(temp_key, v as f64 / 100.0);
        }
        if let Some(v) = sr.humidity {
            let key = if probe {
                format!("machine_probe_humidity_{n}")
            } else {
                format!("ext_humidity_{n}")
            };
            d.fields.insert(key, v as f64 / 2.0);
        }
        if let Some(f) = sr.flags {
            if f & (1 << 0) != 0 {
                event("tilt_alert", json!({ "slot": n }));
            }
        }
        if let Some(v) = sr.illuminance {
            d.fields.insert(format!("ext_illuminance_{n}"), v as f64);
        }
        if let Some(v) = sr.magnetic_field {
            d.fields.insert(format!("ext_magnetic_field_{n}"), v as f64 / 1000.0);
        }
        if let Some(v) = sr.accel_x {
            d.fields.insert(format!("ext_accel_x_{n}"), v as f64 / 100.0);
        }
        if let Some(v) = sr.accel_y {
            d.fields.insert(format!("ext_accel_y_{n}"), v as f64 / 100.0);
        }
        if let Some(v) = sr.accel_z {
            d.fields.insert(format!("ext_accel_z_{n}"), v as f64 / 100.0);
        }
    }

    Ok(d)
}

/// `AlarmEvent.source` enum → label. Mirrors `m_source_names` in firmware
/// `app_alarm_rules.c`.
fn alarm_source_name(s: u32) -> &'static str {
    match s {
        0 => "onboard",
        1 => "s1",
        2 => "s2",
        3 => "s3",
        4 => "s4",
        5 => "hall-left",
        6 => "hall-right",
        7 => "input-a",
        8 => "input-b",
        9 => "pir",
        10 => "accel",
        _ => "?",
    }
}

/// `AlarmEvent.quantity` enum → label. Mirrors `m_quantity_names` in firmware
/// `app_alarm_rules.c`.
fn alarm_quantity_name(q: u32) -> &'static str {
    match q {
        0 => "temperature",
        1 => "humidity",
        2 => "pressure",
        3 => "illuminance",
        4 => "magnetic-field",
        5 => "tilt",
        6 => "state",
        7 => "count",
        _ => "?",
    }
}

/// Rule kind derived from the quantity (`app_alarm_quantity_kind`): tilt/state →
/// state, count → rate, everything else → threshold.
fn alarm_quantity_kind(q: u32) -> &'static str {
    match q {
        5 | 6 => "state",
        7 => "rate",
        _ => "threshold",
    }
}

/// Inverse of firmware `alarm_scale()`: turn the raw wire `value` back into a
/// physical reading for THRESHOLD-kind (analog) quantities. State/rate values
/// are integers (0/1 or a counter) and are returned unscaled.
fn alarm_descale(q: u32, raw: i32) -> serde_json::Value {
    let scaled = |d: f64| json!((raw as f64 / d));
    match q {
        0 | 1 => scaled(100.0),   // temperature, humidity → ×100 on the wire
        2 => scaled(10.0),        // pressure → hPa×10
        4 => scaled(1000.0),      // magnetic-field → µT wire, mT out
        _ => json!(raw),          // illuminance, tilt, state, count → as-is
    }
}

/// Decode a fPort 3 `AlarmReport` frame into a list of alarm events. The rule
/// config is not on the wire — each event carries the resulting edge only.
///
/// Each event is enriched host-side from the firmware enums
/// (`app_alarm_rules.c`): `source`/`quantity`/`edge`/`side` symbolic labels, the
/// rule `kind`, the descaled physical `value` (per `alarm_scale`, analog only)
/// alongside the raw wire value, and the absolute `time` = `base_time + rel_s`.
pub fn decode_alarm_report(bytes: &[u8], received_at: &str) -> Result<Vec<StickerEvent>, String> {
    let r = AlarmReport::decode(bytes).map_err(|e| format!("AlarmReport protobuf decode failed: {e}"))?;
    let events = r
        .events
        .iter()
        .map(|e| {
            let edge = if e.edge == 1 { "deactivate" } else { "activate" };
            let side = match e.side {
                1 => "lo",
                2 => "hi",
                _ => "none",
            };
            let mut extra = json!({
                "source": alarm_source_name(e.source),
                "source_id": e.source,
                "quantity": alarm_quantity_name(e.quantity),
                "quantity_id": e.quantity,
                "kind": alarm_quantity_kind(e.quantity),
                "slot": e.slot,
                "edge": edge,
                "side": side,
                "rel_s": e.rel_s,
                "base_time": r.base_time,
                "time": r.base_time.saturating_add(e.rel_s),
            });
            // value is optional on the wire; descale analog quantities, keep raw.
            if let Some(raw) = e.value {
                extra["value"] = alarm_descale(e.quantity, raw);
                extra["value_raw"] = json!(raw);
            }
            StickerEvent { event_type: "alarm".to_string(), ts: received_at.to_string(), extra }
        })
        .collect();
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::lorawan::sticker_proto::{AlarmEvent, AlarmReport, SensorReading, Telemetry};

    #[test]
    fn telemetry_scaling_matches_ttn_js() {
        let t = Telemetry {
            voltage: Some(150),          // /50 = 3.0 V
            system_flags: Some(0b1),     // bit0 boot
            temperature: Some(2300),     // /100 = 23.0 °C
            humidity: Some(120),         // /2  = 60.0 %
            pressure: Some(10130),       // /10 = 1013.0 hPa
            altitude: Some(2500),        // /10 = 250.0 m
            illuminance: Some(200),      // *2  = 400 lux
            orientation: Some(3),        // event
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
        let t = Telemetry { temperature: Some(-1550), ..Default::default() };
        let d = decode_telemetry(&t.encode_to_vec(), "t").unwrap();
        assert_eq!(d.fields.get("temperature").copied(), Some(-15.5));
    }

    #[test]
    fn telemetry_w1_machine_probe_slot() {
        let t = Telemetry {
            w1_sensors: vec![SensorReading {
                slot: 2,
                r#type: W1_TYPE_MACHINE_PROBE,
                temperature: Some(450), // 4.5 °C
                humidity: Some(140),    // 70 %
                flags: Some(0b1),       // tilt
                ..Default::default()
            }],
            ..Default::default()
        };
        let d = decode_telemetry(&t.encode_to_vec(), "t").unwrap();
        assert_eq!(d.fields.get("machine_probe_temperature_2").copied(), Some(4.5));
        assert_eq!(d.fields.get("machine_probe_humidity_2").copied(), Some(70.0));
        assert!(d.events.iter().any(|e| e.event_type == "tilt_alert"));
    }

    #[test]
    fn alarm_report_decodes_events() {
        // source=1 (s1), quantity=1 (humidity), edge=0 (activate), side=2 (hi),
        // value=5500 → humidity 55.0 %RH (×100 wire). time = base_time + rel_s.
        let r = AlarmReport {
            base_time: 1_780_000_000,
            total: 1,
            events: vec![AlarmEvent {
                source: 1,
                edge: 0,
                side: 2,
                rel_s: 5,
                value: Some(5500),
                quantity: 1,
                slot: 0,
            }],
        };
        let evs = decode_alarm_report(&r.encode_to_vec(), "t").unwrap();
        assert_eq!(evs.len(), 1);
        let e = &evs[0];
        assert_eq!(e.event_type, "alarm");
        assert_eq!(e.extra["source"], "s1");
        assert_eq!(e.extra["quantity"], "humidity");
        assert_eq!(e.extra["kind"], "threshold");
        assert_eq!(e.extra["edge"], "activate");
        assert_eq!(e.extra["side"], "hi");
        assert_eq!(e.extra["slot"], 0);
        assert_eq!(e.extra["value"], 55.0); // descaled ×100
        assert_eq!(e.extra["value_raw"], 5500);
        assert_eq!(e.extra["time"], 1_780_000_005u64);
    }

    #[test]
    fn alarm_descaling_and_kinds_per_quantity() {
        // temperature (q0) ×100 → -5.5 ; pressure (q2) ×10 → 1013.2 ;
        // magnetic-field (q4) ×1000 → 1.5 mT ; state (q6) digital → raw ;
        // count (q7) rate → raw. Also deactivate edge + side none.
        let mk = |quantity, value, edge, side| AlarmEvent {
            source: 0,
            edge,
            side,
            rel_s: 0,
            value: Some(value),
            quantity,
            slot: 0,
        };
        let r = AlarmReport {
            base_time: 0,
            total: 5,
            events: vec![
                mk(0, -550, 1, 0),     // temperature -5.5, deactivate, none
                mk(2, 10132, 0, 1),    // pressure 1013.2, lo
                mk(4, 1500, 0, 2),     // magnetic-field 1.5 mT
                mk(6, 1, 0, 0),        // state digital → 1
                mk(7, 42, 0, 0),       // count rate → 42
            ],
        };
        let evs = decode_alarm_report(&r.encode_to_vec(), "t").unwrap();
        assert_eq!(evs[0].extra["quantity"], "temperature");
        assert_eq!(evs[0].extra["value"], -5.5);
        assert_eq!(evs[0].extra["edge"], "deactivate");
        assert_eq!(evs[0].extra["side"], "none");
        assert_eq!(evs[1].extra["value"], 1013.2);
        assert_eq!(evs[1].extra["side"], "lo");
        assert_eq!(evs[2].extra["quantity"], "magnetic-field");
        assert_eq!(evs[2].extra["value"], 1.5);
        assert_eq!(evs[3].extra["kind"], "state");
        assert_eq!(evs[3].extra["value"], 1);
        assert_eq!(evs[4].extra["kind"], "rate");
        assert_eq!(evs[4].extra["value"], 42);
    }
}
