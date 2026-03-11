//! ChirpStack v4 MQTT uplink parser
//!
//! Parses the JSON envelope published by ChirpStack to MQTT on
//! `application/{app_id}/device/{dev_eui}/event/up`.

use serde_json::Value;

/// Parsed STICKER sensor reading from a ChirpStack uplink
#[derive(Debug, Clone)]
pub struct StickerReading {
    pub dev_eui: String,
    pub device_name: String,
    pub temperature: Option<f32>,
    pub humidity: Option<f32>,
    pub voltage: Option<f32>,
    pub ext_temperature_1: Option<f32>,
    pub ext_temperature_2: Option<f32>,
    pub illuminance: Option<u32>,
    pub motion_count: Option<u32>,
    pub orientation: Option<u8>,
    pub boot: bool,
    pub rssi: Option<i32>,
    pub snr: Option<f32>,
    pub received_at: String,
}

/// Parse a ChirpStack v4 uplink JSON envelope into a StickerReading.
///
/// Expected envelope structure:
/// ```json
/// {
///   "deviceInfo": { "devEui": "...", "deviceName": "..." },
///   "object": { "temperature": 22.5, "humidity": 48.3, ... },
///   "rxInfo": [{ "rssi": -85, "snr": 7.5 }],
///   "time": "2026-03-11T10:30:00Z"
/// }
/// ```
pub fn parse_uplink(payload: &[u8]) -> Result<StickerReading, String> {
    let v: Value = serde_json::from_slice(payload)
        .map_err(|e| format!("Invalid JSON: {}", e))?;

    // Extract device info
    let device_info = v.get("deviceInfo")
        .ok_or("Missing deviceInfo")?;

    let dev_eui = device_info.get("devEui")
        .and_then(|v| v.as_str())
        .ok_or("Missing deviceInfo.devEui")?
        .to_lowercase();

    let device_name = device_info.get("deviceName")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Extract decoded object (STICKER codec output)
    let object = v.get("object").cloned().unwrap_or(Value::Null);

    let temperature = object.get("temperature").and_then(|v| v.as_f64()).map(|v| v as f32);
    let humidity = object.get("humidity").and_then(|v| v.as_f64()).map(|v| v as f32);
    let voltage = object.get("voltage").and_then(|v| v.as_f64()).map(|v| v as f32);
    let ext_temperature_1 = object.get("ext_temperature_1").and_then(|v| v.as_f64()).map(|v| v as f32);
    let ext_temperature_2 = object.get("ext_temperature_2").and_then(|v| v.as_f64()).map(|v| v as f32);
    let illuminance = object.get("illuminance").and_then(|v| v.as_u64()).map(|v| v as u32);
    let motion_count = object.get("motion_count").and_then(|v| v.as_u64()).map(|v| v as u32);
    let orientation = object.get("orientation").and_then(|v| v.as_u64()).map(|v| v as u8);
    let boot = object.get("boot").and_then(|v| v.as_bool()).unwrap_or(false);

    // Extract signal info from first rxInfo entry
    let rx_info = v.get("rxInfo")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first());

    let rssi = rx_info.and_then(|r| r.get("rssi")).and_then(|v| v.as_i64()).map(|v| v as i32);
    let snr = rx_info.and_then(|r| r.get("snr")).and_then(|v| v.as_f64()).map(|v| v as f32);

    // Extract timestamp
    let received_at = v.get("time")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok(StickerReading {
        dev_eui,
        device_name,
        temperature,
        humidity,
        voltage,
        ext_temperature_1,
        ext_temperature_2,
        illuminance,
        motion_count,
        orientation,
        boot,
        rssi,
        snr,
        received_at,
    })
}

/// Extract dev_eui from a ChirpStack MQTT topic.
/// Topic format: `application/{app_id}/device/{dev_eui}/event/up`
pub fn extract_dev_eui_from_topic(topic: &str) -> Option<String> {
    let parts: Vec<&str> = topic.split('/').collect();
    // Expected: ["application", app_id, "device", dev_eui, "event", "up"]
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
    fn test_parse_uplink() {
        let payload = r#"{
            "deviceInfo": {
                "devEui": "70B3D57ED0060ABC",
                "deviceName": "sticker-office-01"
            },
            "object": {
                "boot": false,
                "temperature": 22.5,
                "humidity": 48.3,
                "voltage": 3.1,
                "ext_temperature_1": null,
                "ext_temperature_2": null,
                "illuminance": null,
                "motion_count": null,
                "orientation": null
            },
            "rxInfo": [
                { "rssi": -85, "snr": 7.5 }
            ],
            "time": "2026-03-11T10:30:00Z"
        }"#;

        let reading = parse_uplink(payload.as_bytes()).unwrap();
        assert_eq!(reading.dev_eui, "70b3d57ed0060abc");
        assert_eq!(reading.device_name, "sticker-office-01");
        assert_eq!(reading.temperature, Some(22.5));
        assert_eq!(reading.humidity, Some(48.3));
        assert_eq!(reading.voltage, Some(3.1));
        assert_eq!(reading.rssi, Some(-85));
        assert_eq!(reading.snr, Some(7.5));
        assert!(!reading.boot);
    }

    #[test]
    fn test_parse_uplink_minimal() {
        let payload = r#"{
            "deviceInfo": { "devEui": "AABBCCDD11223344" },
            "object": { "temperature": 20.0 }
        }"#;

        let reading = parse_uplink(payload.as_bytes()).unwrap();
        assert_eq!(reading.dev_eui, "aabbccdd11223344");
        assert_eq!(reading.temperature, Some(20.0));
        assert_eq!(reading.humidity, None);
        assert_eq!(reading.rssi, None);
    }

    #[test]
    fn test_extract_dev_eui_from_topic() {
        let topic = "application/1/device/70b3d57ed0060abc/event/up";
        assert_eq!(extract_dev_eui_from_topic(topic), Some("70b3d57ed0060abc".to_string()));

        let bad_topic = "fiber/device-1/sensors/aggregated";
        assert_eq!(extract_dev_eui_from_topic(bad_topic), None);
    }
}
