//! `{prefix}/{host}/config/state` — retained, QoS 1: authoritative device
//! config snapshot. mqtt.md "config/state" section. This topic is the
//! authoritative Node (LoRaWAN sticker) inventory: a hostname's
//! `lorawan_sensors` list here is the full set, and absence means removed.
//! `eye_tags` is the same thing for the **enrolled** EYE tags: it is the
//! Hub's `eye.tags` (B6, devices.backend.md §22.2), so a registered Beacon
//! missing from it has been deregistered and the portal drops it. It says
//! nothing about *discovered* tags — a stranger the radio overheard is absent
//! from `eye.tags` by definition, and its membership question belongs to the
//! `eye/sensors` snapshot, which is authoritative for that half.
//!
//! **`None` is not `[]`.** Unlike `lorawan_sensors`, the field is an `Option`:
//! `Some([])` is a Hub saying it enrols nothing (prune them all), `None` is a
//! document making no statement — older firmware omits the key, and so does a
//! Hub that could not read its own `eye:` section
//! (`control-core`'s `ble_host::config_store::enrolled_tags`).
//!
//! Cross-checked against `handle_device_config_state`
//! (`handlers.py:1860-2038`): every nested container (`eye`, per-sensor
//! `thresholds`, `intervals`) is read as `data.get(key) or {}` /
//! `data.get(key, {})`, i.e. optional; `sensors`/`lorawan_sensors` default
//! to `[]` when absent (`#[serde(default)]`); `eye_tags` is checked with
//! `isinstance(..., list)` (older firmware omits the key entirely).

use serde::{Deserialize, Serialize};

use crate::payloads::lorawan::FieldThreshold;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigStateMsg {
    pub timestamp: Option<String>,
    pub led_brightness: Option<i64>,
    pub screen_brightness: Option<i64>,
    pub buzzer_volume: Option<i64>,
    pub screen_timeout_secs: Option<i64>,
    pub system_info_interval_s: Option<i64>,
    pub device_label: Option<String>,
    /// The Hub's configured LCD rows (display-config.md §5). `None` is a
    /// document making no statement — which is every Hub today: the agent's
    /// `build_config_state_payload` does not emit the key yet, only the dev
    /// mock does. `Some([])` is a Hub saying it renders the built-in
    /// layout.
    pub display_lines: Option<Vec<crate::display::DisplayLine>>,
    pub eye: Option<EyeConfig>,
    #[serde(default)]
    pub sensors: Vec<SensorConfigEntry>,
    #[serde(default)]
    pub lorawan_sensors: Vec<LorawanSensorConfigEntry>,
    pub eye_tags: Option<Vec<EyeTagConfigEntry>>,
    pub intervals: Option<Intervals>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EyeConfig {
    pub enabled: Option<bool>,
    pub auto_provision: Option<bool>,
    pub auto_discover: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorConfigEntry {
    #[serde(default)]
    pub line: i64,
    pub name: Option<String>,
    pub location: Option<String>,
    pub enabled: Option<bool>,
    pub has_override: Option<bool>,
    pub thresholds: Option<SensorThresholds>,
}

/// Wired-probe alarm thresholds. Device wire keys are `*_celsius`-suffixed
/// and don't line up 1:1 with the portal's naming; mapped here via
/// `#[serde(rename)]` per `handlers.py:1915-1922`:
///
/// | Wire key                 | Portal field    |
/// |---------------------------|-----------------|
/// | `critical_low_celsius`    | `critical_low`  |
/// | `critical_high_celsius`   | `critical_high` |
/// | `low_alarm_celsius`       | `alarm_low`      |
/// | `high_alarm_celsius`      | `alarm_high`     |
/// | `warning_low_celsius`     | `warning_low`    |
/// | `warning_high_celsius`    | `warning_high`   |
/// | `hysteresis_celsius`      | `hysteresis`     |
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SensorThresholds {
    #[serde(rename = "critical_low_celsius")]
    pub critical_low: Option<f64>,
    #[serde(rename = "critical_high_celsius")]
    pub critical_high: Option<f64>,
    #[serde(rename = "low_alarm_celsius")]
    pub alarm_low: Option<f64>,
    #[serde(rename = "high_alarm_celsius")]
    pub alarm_high: Option<f64>,
    #[serde(rename = "warning_low_celsius")]
    pub warning_low: Option<f64>,
    #[serde(rename = "warning_high_celsius")]
    pub warning_high: Option<f64>,
    /// The hysteresis the Hub grades the line with locally (decision #41), echoing the
    /// portal's; absent from an older Hub.
    #[serde(
        rename = "hysteresis_celsius",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub hysteresis: Option<f64>,
    /// The band's "Mez zapnuta" switch (#167); absent (an older Hub) = on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LorawanSensorConfigEntry {
    pub dev_eui: String,
    pub name: Option<String>,
    pub serial_number: Option<String>,
    pub location: Option<String>,
    pub enabled: Option<bool>,
    pub temp_critical_low: Option<f64>,
    pub temp_warning_low: Option<f64>,
    pub temp_warning_high: Option<f64>,
    pub temp_critical_high: Option<f64>,
    pub humidity_critical_low: Option<f64>,
    pub humidity_warning_low: Option<f64>,
    pub humidity_warning_high: Option<f64>,
    pub humidity_critical_high: Option<f64>,
    /// Present ⇒ authoritative for this Node (mqtt.md `lorawan/sensors`
    /// section, `thresholds_authoritative` check at `handlers.py:2610`).
    pub field_thresholds: Option<Vec<FieldThreshold>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EyeTagConfigEntry {
    pub mac: String,
    pub name: Option<String>,
    pub location: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intervals {
    pub sample_interval_ms: Option<i64>,
    pub aggregation_interval_ms: Option<i64>,
    pub report_interval_ms: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #167: an older Hub sends no `enabled`; a newer one echoes the switch.
    #[test]
    fn threshold_enabled_is_optional() {
        let old: SensorThresholds =
            serde_json::from_str(r#"{"critical_low_celsius":2,"critical_high_celsius":10}"#)
                .unwrap();
        assert_eq!(old.enabled, None);
        let off: SensorThresholds =
            serde_json::from_str(r#"{"critical_high_celsius":10,"enabled":false}"#).unwrap();
        assert_eq!(off.enabled, Some(false));
    }

    #[test]
    fn deserializes_spec_example() {
        let json = r#"{
            "timestamp": "2026-01-13T14:30:00Z",
            "led_brightness": 75,
            "screen_brightness": 100,
            "buzzer_volume": 50,
            "screen_timeout_secs": 30,
            "system_info_interval_s": 60,
            "device_label": "Freezer 01",
            "display_lines": [ { "source": "ds18b20", "line": 0, "field": "temperature" } ],
            "eye": { "enabled": true, "auto_provision": false, "auto_discover": true },
            "sensors": [
                {
                    "line": 0, "name": "Sensor 1", "location": "Shelf A",
                    "enabled": true, "has_override": false,
                    "thresholds": {
                        "critical_low_celsius": -20.0, "low_alarm_celsius": -15.0,
                        "warning_low_celsius": -10.0, "warning_high_celsius": 5.0,
                        "high_alarm_celsius": 8.0, "critical_high_celsius": 10.0
                    }
                }
            ],
            "lorawan_sensors": [
                {
                    "dev_eui": "0011223344556677", "name": "Node 1", "serial_number": "SN1",
                    "location": "Room A", "enabled": true,
                    "temp_critical_low": -20.0, "temp_warning_low": -10.0,
                    "temp_warning_high": 5.0, "temp_critical_high": 10.0,
                    "humidity_critical_low": 10.0, "humidity_warning_low": 20.0,
                    "humidity_warning_high": 80.0, "humidity_critical_high": 90.0,
                    "field_thresholds": []
                }
            ],
            "eye_tags": [ { "mac": "AA:BB:CC:DD:EE:FF", "name": "Tag 1", "location": "Room B" } ],
            "intervals": { "sample_interval_ms": 10000, "aggregation_interval_ms": 60000, "report_interval_ms": 60000 }
        }"#;
        let msg: ConfigStateMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg.sensors.len(), 1);
        let thr = msg.sensors[0].thresholds.as_ref().unwrap();
        assert_eq!(thr.critical_low, Some(-20.0));
        assert_eq!(thr.alarm_low, Some(-15.0));
        assert_eq!(thr.alarm_high, Some(8.0));
        assert_eq!(msg.lorawan_sensors.len(), 1);
        assert_eq!(msg.lorawan_sensors[0].dev_eui, "0011223344556677");
        assert_eq!(msg.eye_tags.as_ref().unwrap().len(), 1);
        assert_eq!(msg.display_lines.as_ref().unwrap().len(), 1);
        assert_eq!(
            msg.intervals.as_ref().unwrap().sample_interval_ms,
            Some(10000)
        );
    }

    #[test]
    fn missing_sections_default_empty() {
        let msg: ConfigStateMsg = serde_json::from_str("{}").unwrap();
        assert!(msg.sensors.is_empty());
        assert!(msg.lorawan_sensors.is_empty());
        assert!(msg.eye_tags.is_none());
        assert!(msg.display_lines.is_none());
        assert!(msg.intervals.is_none());
    }
}
