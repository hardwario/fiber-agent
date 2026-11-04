//! `{prefix}/{host}/sensors/aggregated` — wired probe telemetry (~60s, not
//! retained). mqtt.md "sensors/aggregated" section.
//!
//! Cross-checked against `handle_sensors_aggregated`
//! (`handlers.py:560-659`): `line`/`sample_count`/`disconnected_count`
//! default to `0` when absent, `dominant_alarm_state` defaults to
//! `"NORMAL"`, `temperature`/`alarm_counts` default to `{}` when the key is
//! missing or null (`sensor_data.get('temperature') or {}`), hence
//! `Option` containers here.

use serde::{Deserialize, Serialize};

fn default_normal() -> String {
    "NORMAL".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorsAggregatedMsg {
    pub timestamp: Option<String>,
    pub period_start_ts: Option<i64>,
    pub period_end_ts: Option<i64>,
    pub duration_sec: Option<i64>,
    #[serde(default)]
    pub sensors: Vec<AggregatedSensor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregatedSensor {
    #[serde(default)]
    pub line: i64,
    pub name: Option<String>,
    pub location: Option<String>,
    #[serde(default)]
    pub sample_count: i64,
    #[serde(default)]
    pub disconnected_count: i64,
    pub temperature: Option<TemperatureStats>,
    pub alarm_counts: Option<AlarmCounts>,
    #[serde(default = "default_normal")]
    pub dominant_alarm_state: String,
    pub alarm_triggered_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemperatureStats {
    pub min_celsius: Option<f64>,
    pub max_celsius: Option<f64>,
    pub avg_celsius: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlarmCounts {
    pub normal: Option<i64>,
    pub warning: Option<i64>,
    pub critical: Option<i64>,
    pub disconnected: Option<i64>,
    pub reconnecting: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_spec_example() {
        let json = r#"{
            "timestamp": "2026-01-13T14:30:00Z",
            "period_start_ts": 1736775000,
            "period_end_ts": 1736775060,
            "duration_sec": 60,
            "sensors": [
                {
                    "line": 0,
                    "name": "Sensor 1",
                    "location": null,
                    "sample_count": 60,
                    "disconnected_count": 0,
                    "temperature": { "min_celsius": 36.5, "max_celsius": 37.2, "avg_celsius": 36.8 },
                    "alarm_counts": { "normal": 60, "warning": 0, "critical": 0, "disconnected": 0, "reconnecting": 0 },
                    "dominant_alarm_state": "NORMAL",
                    "alarm_triggered_at": null
                }
            ]
        }"#;
        let msg: SensorsAggregatedMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg.sensors.len(), 1);
        let s = &msg.sensors[0];
        assert_eq!(s.line, 0);
        assert_eq!(s.name.as_deref(), Some("Sensor 1"));
        assert_eq!(s.temperature.as_ref().unwrap().avg_celsius, Some(36.8));
        assert_eq!(s.dominant_alarm_state, "NORMAL");
    }

    #[test]
    fn tolerates_missing_optional_fields_and_unknown_keys() {
        let json = r#"{ "sensors": [ { "unexpected_field": 1 } ] }"#;
        let msg: SensorsAggregatedMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg.sensors.len(), 1);
        assert_eq!(msg.sensors[0].line, 0);
        assert_eq!(msg.sensors[0].dominant_alarm_state, "NORMAL");
        assert!(msg.sensors[0].temperature.is_none());
    }
}
