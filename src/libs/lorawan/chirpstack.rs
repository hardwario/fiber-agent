//! ChirpStack v4 MQTT uplink parser → generic StickerReading
//!
//! Decoding happens **in-app** (issue #31): we read the raw application payload
//! (`data`, base64) from the ChirpStack uplink and dispatch by `fPort` into the
//! protobuf codec (`super::sticker_payload`). The ChirpStack device-profile JS
//! codec is no longer the source of truth; its decoded `object` is only used as
//! a migration fallback until every STICKER runs firmware v1.4.0.

use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

use super::registry::{REGISTRY, FieldKind};
use super::sticker_payload;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StickerEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub ts: String,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct StickerReading {
    pub dev_eui: String,
    pub device_name: String,
    pub fields: HashMap<String, f64>,
    pub counters: HashMap<String, u64>,
    pub events: Vec<StickerEvent>,
    pub rssi: Option<i32>,
    pub snr: Option<f32>,
    pub received_at: String,
}

/// Parse a ChirpStack v4 uplink event into a `StickerReading`.
///
/// Decoding is in-app (issue #31): the raw application payload (`data`, base64)
/// is decoded by `fPort` through the protobuf codec:
///   - fPort 2  → `Telemetry`   (#64)
///   - fPort 3  → `AlarmReport` (#77)
///   - fPort 85 → command/response, correlated by the seq stream (#34) → skipped
///   - other    → logged and skipped (forward-compat)
///
/// While devices migrate to firmware v1.4.0, fPort 1 / no-fPort uplinks fall back
/// to the legacy ChirpStack JS-codec `object`. Returns `Ok(None)` when the uplink
/// carries nothing this monitor should persist.
pub fn parse_uplink(payload: &[u8]) -> Result<Option<StickerReading>, String> {
    let v: Value = serde_json::from_slice(payload).map_err(|e| format!("Invalid JSON: {}", e))?;
    let device_info = v.get("deviceInfo").ok_or("Missing deviceInfo")?;
    let dev_eui = device_info.get("devEui")
        .and_then(|v| v.as_str())
        .ok_or("Missing deviceInfo.devEui")?
        .to_lowercase();
    let device_name = device_info.get("deviceName")
        .and_then(|v| v.as_str()).unwrap_or("").to_string();
    let received_at = v.get("time").and_then(|v| v.as_str()).unwrap_or("").to_string();

    let rx_info = v.get("rxInfo").and_then(|v| v.as_array()).and_then(|arr| arr.first());
    let rssi = rx_info.and_then(|r| r.get("rssi")).and_then(|v| v.as_i64()).map(|v| v as i32);
    let snr = rx_info.and_then(|r| r.get("snr")).and_then(|v| v.as_f64()).map(|v| v as f32);

    let mut fields = HashMap::new();
    let mut counters = HashMap::new();
    let mut events = Vec::new();

    // LoRaWAN frame counter → message_id dedup (UNIQUE on sticker_readings.message_id).
    if let Some(fcnt) = v.get("fCnt").and_then(|v| v.as_u64()) {
        counters.insert("fCnt".to_string(), fcnt);
    }

    let fport = v.get("fPort").and_then(|v| v.as_u64());
    let data_b64 = v.get("data").and_then(|v| v.as_str());

    match (fport, data_b64) {
        // fPort 2: protobuf Telemetry (#64)
        (Some(2), Some(b64)) => {
            let bytes = BASE64.decode(b64).map_err(|e| format!("Invalid base64 data: {}", e))?;
            let d = sticker_payload::decode_telemetry(&bytes, &received_at)?;
            fields.extend(d.fields);
            counters.extend(d.counters);
            events.extend(d.events);
        }
        // fPort 3: protobuf AlarmReport (#77)
        (Some(3), Some(b64)) => {
            let bytes = BASE64.decode(b64).map_err(|e| format!("Invalid base64 data: {}", e))?;
            events.extend(sticker_payload::decode_alarm_report(&bytes, &received_at)?);
        }
        // fPort 85: command/response, handled by the seq-correlation stream (#34).
        (Some(85), _) => return Ok(None),
        // Legacy fPort 1 / no fPort: fall back to the JS-codec `object` during
        // migration. Skip if there is nothing to decode.
        (Some(1), _) | (None, _) => {
            let object = v.get("object").cloned().unwrap_or(Value::Null);
            if object.is_null() {
                return Ok(None);
            }
            decode_object_legacy(&object, &received_at, &mut fields, &mut counters, &mut events);
        }
        // Any other fPort is unknown to this monitor: log + skip (forward-compat).
        (Some(other), _) => {
            eprintln!("[LoRaWAN] {}: skipping uplink on unhandled fPort {}", dev_eui, other);
            return Ok(None);
        }
    }

    Ok(Some(StickerReading {
        dev_eui, device_name, fields, counters, events, rssi, snr, received_at,
    }))
}

/// Legacy path: map the ChirpStack JS-codec `object` (flat fields) into
/// fields/counters/events. Kept as a migration fallback until every STICKER runs
/// firmware v1.4.0 (protobuf on fPort 2/3); see issue #31.
fn decode_object_legacy(
    object: &Value,
    received_at: &str,
    fields: &mut HashMap<String, f64>,
    counters: &mut HashMap<String, u64>,
    events: &mut Vec<StickerEvent>,
) {
    // Iterate the registry: assign each known field to fields/counters
    for fdef in REGISTRY {
        let Some(val) = object.get(fdef.name) else { continue; };
        if val.is_null() { continue; }
        match fdef.kind {
            FieldKind::Continuous => {
                if let Some(n) = val.as_f64() {
                    fields.insert(fdef.name.to_string(), n);
                } else if let Some(n) = val.as_u64() {
                    fields.insert(fdef.name.to_string(), n as f64);
                } else if let Some(n) = val.as_i64() {
                    fields.insert(fdef.name.to_string(), n as f64);
                }
            }
            FieldKind::Counter => {
                if let Some(n) = val.as_u64() {
                    counters.insert(fdef.name.to_string(), n);
                }
            }
            FieldKind::Event => {}
        }
    }

    let mut push_event = |event_type: &str, extra: serde_json::Value| {
        events.push(StickerEvent {
            event_type: event_type.to_string(),
            ts: received_at.to_string(),
            extra,
        });
    };

    // Events: boot/orientation/tilt/hall/input flags
    if object.get("boot").and_then(|v| v.as_bool()).unwrap_or(false) {
        push_event("boot", serde_json::json!({}));
    }
    if let Some(o) = object.get("orientation").and_then(|v| v.as_u64()) {
        push_event("orientation", serde_json::json!({"value": o}));
    }
    if object.get("machine_probe_tilt_alert_1").and_then(|v| v.as_bool()).unwrap_or(false) {
        push_event("tilt_alert_1", serde_json::json!({}));
    }
    if object.get("machine_probe_tilt_alert_2").and_then(|v| v.as_bool()).unwrap_or(false) {
        push_event("tilt_alert_2", serde_json::json!({}));
    }
    for (channel, key) in [("left", "hall_left_is_active"), ("right", "hall_right_is_active")] {
        if object.get(key).and_then(|v| v.as_bool()).unwrap_or(false) {
            push_event("hall_active", serde_json::json!({"channel": channel, "active": true}));
        }
    }
    for (channel, key, kind) in [
        ("left",  "hall_left_notify_act",    "act"),
        ("left",  "hall_left_notify_deact",  "deact"),
        ("right", "hall_right_notify_act",   "act"),
        ("right", "hall_right_notify_deact", "deact"),
    ] {
        if object.get(key).and_then(|v| v.as_bool()).unwrap_or(false) {
            push_event("hall_notify", serde_json::json!({"channel": channel, "kind": kind}));
        }
    }
    for (channel, key) in [("a", "input_a_is_active"), ("b", "input_b_is_active")] {
        if object.get(key).and_then(|v| v.as_bool()).unwrap_or(false) {
            push_event("input_active", serde_json::json!({"channel": channel, "active": true}));
        }
    }
    for (channel, key, kind) in [
        ("a", "input_a_notify_act",    "act"),
        ("a", "input_a_notify_deact",  "deact"),
        ("b", "input_b_notify_act",    "act"),
        ("b", "input_b_notify_deact",  "deact"),
    ] {
        if object.get(key).and_then(|v| v.as_bool()).unwrap_or(false) {
            push_event("input_notify", serde_json::json!({"channel": channel, "kind": kind}));
        }
    }
}

/// Compute a stable message_id for a sticker uplink.
///
/// Format: `{dev_eui}-{ts}-{seq}`. `seq` is `fCnt` if present in the
/// `counters` map (inserted by `parse_uplink` from the ChirpStack uplink's
/// LoRaWAN frame counter), otherwise 0. Two uplinks from the same `dev_eui`
/// with the same `(ts, seq)` are treated as the same message (the
/// save-and-feed write path dedups via the UNIQUE constraint on
/// `sticker_readings.message_id`).
pub fn message_id_for(reading: &StickerReading, ts: i64) -> String {
    let seq = reading.counters.get("fCnt").copied().unwrap_or(0);
    format!("{}-{}-{}", reading.dev_eui, ts, seq)
}

/// Extract dev_eui from a ChirpStack MQTT topic.
pub fn extract_dev_eui_from_topic(topic: &str) -> Option<String> {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() >= 6 && parts[0] == "application" && parts[2] == "device" && parts[4] == "event" {
        Some(parts[3].to_lowercase())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::lorawan::sticker_proto::{AlarmEvent, AlarmReport, Telemetry};
    use prost::Message;

    /// Wrap protobuf bytes in a minimal ChirpStack v4 uplink JSON on `fPort`.
    fn chirpstack_uplink(dev_eui: &str, fport: u64, fcnt: u64, proto: &[u8]) -> String {
        let data = BASE64.encode(proto);
        serde_json::json!({
            "deviceInfo": { "devEui": dev_eui, "deviceName": "sticker-01" },
            "fPort": fport,
            "fCnt": fcnt,
            "data": data,
            "rxInfo": [{ "rssi": -85, "snr": 7.5 }],
            "time": "2026-06-19T10:30:00Z",
        }).to_string()
    }

    #[test]
    fn dispatch_fport2_telemetry_decodes_in_app() {
        let t = Telemetry {
            voltage: Some(150),       // /50 = 3.0 V
            temperature: Some(2300),  // /100 = 23.0 °C
            humidity: Some(120),      // /2  = 60.0 %
            motion_count: Some(7),
            system_flags: Some(0b1),  // boot
            ..Default::default()
        };
        let payload = chirpstack_uplink("70B3D57ED0060ABC", 2, 11, &t.encode_to_vec());
        let r = parse_uplink(payload.as_bytes()).unwrap().expect("reading");
        assert_eq!(r.dev_eui, "70b3d57ed0060abc");
        assert_eq!(r.fields.get("temperature").copied(), Some(23.0));
        assert_eq!(r.fields.get("voltage").copied(), Some(3.0));
        assert_eq!(r.fields.get("humidity").copied(), Some(60.0));
        assert_eq!(r.counters.get("motion_count").copied(), Some(7));
        assert_eq!(r.counters.get("fCnt").copied(), Some(11));
        assert!(r.events.iter().any(|e| e.event_type == "boot"));
        assert_eq!(r.rssi, Some(-85));
    }

    #[test]
    fn dispatch_fport3_alarm_report_decodes_in_app() {
        let report = AlarmReport {
            base_time: 1_780_000_000,
            total: 1,
            events: vec![AlarmEvent {
                source: 1, edge: 0, side: 2, rel_s: 5,
                value: Some(5500), quantity: 1, slot: 0,
            }],
        };
        let payload = chirpstack_uplink("aabb", 3, 4, &report.encode_to_vec());
        let r = parse_uplink(payload.as_bytes()).unwrap().expect("reading");
        assert_eq!(r.events.len(), 1);
        assert_eq!(r.events[0].event_type, "alarm");
        assert_eq!(r.events[0].extra["value"], 5500);
    }

    #[test]
    fn dispatch_fport85_is_skipped() {
        // fPort 85 carries command/response, correlated elsewhere (#34).
        let payload = chirpstack_uplink("aabb", 85, 1, &[0x01, 0x02]);
        assert!(parse_uplink(payload.as_bytes()).unwrap().is_none());
    }

    #[test]
    fn dispatch_unknown_fport_is_skipped() {
        let payload = chirpstack_uplink("aabb", 42, 1, &[0x00]);
        assert!(parse_uplink(payload.as_bytes()).unwrap().is_none());
    }

    #[test]
    fn legacy_object_fallback_without_fport() {
        let payload = r#"{
            "deviceInfo": { "devEui": "70B3D57ED0060ABC", "deviceName": "sticker-01" },
            "object": {
                "boot": true,
                "temperature": 22.5,
                "humidity": 48.3,
                "ext_temperature_1": 18.0,
                "pressure": 101325,
                "motion_count": 42,
                "hall_left_count": 7,
                "hall_left_is_active": true,
                "machine_probe_tilt_alert_1": true,
                "orientation": 2
            },
            "rxInfo": [{ "rssi": -85, "snr": 7.5 }],
            "time": "2026-05-12T10:30:00Z"
        }"#;
        let r = parse_uplink(payload.as_bytes()).unwrap().expect("reading");
        assert_eq!(r.dev_eui, "70b3d57ed0060abc");
        assert_eq!(r.fields.get("temperature").copied(), Some(22.5));
        assert_eq!(r.fields.get("ext_temperature_1").copied(), Some(18.0));
        assert_eq!(r.fields.get("pressure").copied(), Some(101325.0));
        assert_eq!(r.counters.get("motion_count").copied(), Some(42));
        assert_eq!(r.counters.get("hall_left_count").copied(), Some(7));
        assert!(r.events.iter().any(|e| e.event_type == "boot"));
        assert!(r.events.iter().any(|e| e.event_type == "tilt_alert_1"));
        assert!(r.events.iter().any(|e| e.event_type == "hall_active"));
        assert!(r.events.iter().any(|e| e.event_type == "orientation"));
    }

    #[test]
    fn legacy_object_null_fields_are_omitted() {
        let payload = r#"{
            "deviceInfo": { "devEui": "AABB" },
            "object": { "temperature": 20.0, "humidity": null }
        }"#;
        let r = parse_uplink(payload.as_bytes()).unwrap().expect("reading");
        assert!(r.fields.contains_key("temperature"));
        assert!(!r.fields.contains_key("humidity"));
    }

    #[test]
    fn empty_uplink_without_data_or_object_is_skipped() {
        let payload = r#"{ "deviceInfo": { "devEui": "AABB" } }"#;
        assert!(parse_uplink(payload.as_bytes()).unwrap().is_none());
    }

    #[test]
    fn message_id_uses_fcnt_when_present_else_received_at_seq() {
        let mut r = StickerReading {
            dev_eui: "70b3d5".into(),
            device_name: "".into(),
            fields: Default::default(),
            counters: Default::default(),
            events: vec![],
            rssi: None,
            snr: None,
            received_at: "2026-05-19T12:00:00Z".into(),
        };
        r.counters.insert("fCnt".into(), 42);
        assert_eq!(message_id_for(&r, 1716120000), "70b3d5-1716120000-42");

        r.counters.remove("fCnt");
        assert_eq!(message_id_for(&r, 1716120000), "70b3d5-1716120000-0");
    }

    #[test]
    fn test_extract_dev_eui_from_topic() {
        let topic = "application/1/device/70b3d57ed0060abc/event/up";
        assert_eq!(extract_dev_eui_from_topic(topic), Some("70b3d57ed0060abc".to_string()));
        let bad_topic = "fiber/device-1/sensors/aggregated";
        assert_eq!(extract_dev_eui_from_topic(bad_topic), None);
    }
}
