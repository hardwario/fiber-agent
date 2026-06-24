//! ChirpStack v4 MQTT uplink parser → generic StickerReading

use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::registry::{REGISTRY, FieldKind};

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

pub fn parse_uplink(payload: &[u8]) -> Result<StickerReading, String> {
    let v: Value = serde_json::from_slice(payload).map_err(|e| format!("Invalid JSON: {}", e))?;
    let device_info = v.get("deviceInfo").ok_or("Missing deviceInfo")?;
    let dev_eui = device_info.get("devEui")
        .and_then(|v| v.as_str())
        .ok_or("Missing deviceInfo.devEui")?
        .to_lowercase();
    let device_name = device_info.get("deviceName")
        .and_then(|v| v.as_str()).unwrap_or("").to_string();
    let object = v.get("object").cloned().unwrap_or(Value::Null);
    let received_at = v.get("time").and_then(|v| v.as_str()).unwrap_or("").to_string();

    let mut fields = HashMap::new();
    let mut counters = HashMap::new();
    let mut events = Vec::new();

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

    // Events: boot/orientation/tilt/hall/input flags
    if object.get("boot").and_then(|v| v.as_bool()).unwrap_or(false) {
        events.push(StickerEvent {
            event_type: "boot".into(),
            ts: received_at.clone(),
            extra: serde_json::json!({}),
        });
    }
    if let Some(o) = object.get("orientation").and_then(|v| v.as_u64()) {
        events.push(StickerEvent {
            event_type: "orientation".into(),
            ts: received_at.clone(),
            extra: serde_json::json!({"value": o}),
        });
    }
    if object.get("machine_probe_tilt_alert_1").and_then(|v| v.as_bool()).unwrap_or(false) {
        events.push(StickerEvent {
            event_type: "tilt_alert_1".into(), ts: received_at.clone(),
            extra: serde_json::json!({}),
        });
    }
    if object.get("machine_probe_tilt_alert_2").and_then(|v| v.as_bool()).unwrap_or(false) {
        events.push(StickerEvent {
            event_type: "tilt_alert_2".into(), ts: received_at.clone(),
            extra: serde_json::json!({}),
        });
    }
    for (channel, key) in [("left", "hall_left_is_active"), ("right", "hall_right_is_active")] {
        if object.get(key).and_then(|v| v.as_bool()).unwrap_or(false) {
            events.push(StickerEvent {
                event_type: "hall_active".into(), ts: received_at.clone(),
                extra: serde_json::json!({"channel": channel, "active": true}),
            });
        }
    }
    for (channel, key, kind) in [
        ("left",  "hall_left_notify_act",    "act"),
        ("left",  "hall_left_notify_deact",  "deact"),
        ("right", "hall_right_notify_act",   "act"),
        ("right", "hall_right_notify_deact", "deact"),
    ] {
        if object.get(key).and_then(|v| v.as_bool()).unwrap_or(false) {
            events.push(StickerEvent {
                event_type: "hall_notify".into(), ts: received_at.clone(),
                extra: serde_json::json!({"channel": channel, "kind": kind}),
            });
        }
    }
    for (channel, key) in [("a", "input_a_is_active"), ("b", "input_b_is_active")] {
        if object.get(key).and_then(|v| v.as_bool()).unwrap_or(false) {
            events.push(StickerEvent {
                event_type: "input_active".into(), ts: received_at.clone(),
                extra: serde_json::json!({"channel": channel, "active": true}),
            });
        }
    }
    for (channel, key, kind) in [
        ("a", "input_a_notify_act",    "act"),
        ("a", "input_a_notify_deact",  "deact"),
        ("b", "input_b_notify_act",    "act"),
        ("b", "input_b_notify_deact",  "deact"),
    ] {
        if object.get(key).and_then(|v| v.as_bool()).unwrap_or(false) {
            events.push(StickerEvent {
                event_type: "input_notify".into(), ts: received_at.clone(),
                extra: serde_json::json!({"channel": channel, "kind": kind}),
            });
        }
    }

    let rx_info = v.get("rxInfo").and_then(|v| v.as_array()).and_then(|arr| arr.first());
    let rssi = rx_info.and_then(|r| r.get("rssi")).and_then(|v| v.as_i64()).map(|v| v as i32);
    let snr = rx_info.and_then(|r| r.get("snr")).and_then(|v| v.as_f64()).map(|v| v as f32);

    // LoRaWAN frame counter — ChirpStack v4 surfaces this at the top level of
    // the uplink event envelope. Inserted into `counters` so `message_id_for`
    // can include it in the row identity; without it, two uplinks landing in
    // the same UNIX second collide on `sticker_readings.message_id` and the
    // second is silently dropped by INSERT OR IGNORE.
    if let Some(f_cnt) = v.get("fCnt").and_then(|x| x.as_u64()) {
        counters.insert("fCnt".to_string(), f_cnt);
    }

    Ok(StickerReading {
        dev_eui, device_name, fields, counters, events, rssi, snr, received_at,
    })
}

/// Compute a stable message_id for a sticker uplink.
///
/// Format: `{dev_eui}-{ts}-{seq}`. `seq` is `fCnt` if present in the
/// `counters` map (it's expected to be inserted by `parse_uplink` when the
/// LoRaWAN frame counter is available), otherwise 0. Two uplinks from the
/// same `dev_eui` with the same `(ts, seq)` are treated as the same
/// message (the save-and-feed write path dedups via the UNIQUE constraint
/// on `sticker_readings.message_id`).
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

    #[test]
    fn test_parse_v2_fields_counters_events() {
        let payload = r#"{
            "deviceInfo": { "devEui": "70B3D57ED0060ABC", "deviceName": "sticker-01" },
            "object": {
                "boot": true,
                "temperature": 22.5,
                "humidity": 48.3,
                "voltage": 3.1,
                "ext_temperature_1": 18.0,
                "pressure": 101325,
                "altitude": 230.5,
                "motion_count": 42,
                "hall_left_count": 7,
                "hall_left_is_active": true,
                "machine_probe_tilt_alert_1": true,
                "orientation": 2
            },
            "rxInfo": [{ "rssi": -85, "snr": 7.5 }],
            "time": "2026-05-12T10:30:00Z"
        }"#;
        let r = parse_uplink(payload.as_bytes()).unwrap();
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
    fn test_parse_null_fields_are_omitted() {
        let payload = r#"{
            "deviceInfo": { "devEui": "AABB" },
            "object": { "temperature": 20.0, "humidity": null }
        }"#;
        let r = parse_uplink(payload.as_bytes()).unwrap();
        assert!(r.fields.contains_key("temperature"));
        assert!(!r.fields.contains_key("humidity"));
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
    fn parse_uplink_extracts_fcnt_into_counters() {
        let payload = r#"{
            "deviceInfo": { "devEui": "AABB" },
            "fCnt": 137,
            "object": { "temperature": 20.0 },
            "time": "2026-05-19T12:00:00Z"
        }"#;
        let r = parse_uplink(payload.as_bytes()).unwrap();
        assert_eq!(r.counters.get("fCnt").copied(), Some(137));
        // And the message_id derived from it must include the counter so two
        // uplinks within the same UNIX second don't collide on the UNIQUE
        // constraint in sticker_readings.
        assert_eq!(message_id_for(&r, 1716120000), "aabb-1716120000-137");
    }

    #[test]
    fn test_extract_dev_eui_from_topic() {
        let topic = "application/1/device/70b3d57ed0060abc/event/up";
        assert_eq!(extract_dev_eui_from_topic(topic), Some("70b3d57ed0060abc".to_string()));
        let bad_topic = "fiber/device-1/sensors/aggregated";
        assert_eq!(extract_dev_eui_from_topic(bad_topic), None);
    }
}
