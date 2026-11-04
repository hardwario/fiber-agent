//! `{prefix}/{host}/eye/aggregated` — Beacon window aggregates (QoS 1, not
//! retained), the Beacon analogue of `sensors/aggregated`
//! (`plan/control/storage_intervals.md` §5).
//!
//! Published once per `eye.aggregation_interval_s` window (600 s by
//! default) with every Beacon that had frames in it. Each entry is the same
//! data as the Hub's `advertising` row in `eye_readings`, and carries that
//! row's `message_id` (`{MAC}-adv-{window_start}`) so the portal dedups on
//! the Hub's own identity for the window. The producer is
//! `control_ble::host::AggregatedSnapshot`; this is the portal's lenient
//! mirror of it.
//!
//! Field keys and units: `temperature` °C, `humidity` %, `battery` mV,
//! `rssi` dBm, `pitch` / `roll` degrees, `movement_count` the Beacon's
//! counter. A field no frame carried is absent.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

fn default_normal() -> String {
    "NORMAL".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EyeAggregatedMsg {
    pub timestamp: Option<String>,
    pub period_start_ts: Option<i64>,
    pub period_end_ts: Option<i64>,
    pub duration_sec: Option<i64>,
    #[serde(default)]
    pub tags: Vec<EyeAggregatedTag>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EyeAggregatedTag {
    pub mac: String,
    pub name: Option<String>,
    /// `{MAC}-adv-{window_start}`.
    pub message_id: Option<String>,
    pub window_start: Option<i64>,
    pub window_s: Option<i64>,
    #[serde(default)]
    pub count: i64,
    /// The worst alarm level of the window's frames.
    #[serde(default = "default_normal")]
    pub alarm_state: String,
    #[serde(default)]
    pub fields: BTreeMap<String, FieldStats>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FieldStats {
    pub min: Option<f64>,
    pub avg: Option<f64>,
    pub max: Option<f64>,
}

impl EyeAggregatedTag {
    /// The plotted value of one field: the window average.
    pub fn avg(&self, field: &str) -> Option<f64> {
        self.fields.get(field).and_then(|f| f.avg)
    }

    /// The lowest value of one field in the window.
    pub fn min(&self, field: &str) -> Option<f64> {
        self.fields.get(field).and_then(|f| f.min)
    }

    /// The highest value of one field in the window.
    pub fn max(&self, field: &str) -> Option<f64> {
        self.fields.get(field).and_then(|f| f.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_the_documented_shape() {
        let json = r#"{
            "timestamp": "2026-10-12T10:10:00Z",
            "period_start_ts": 1791799200,
            "period_end_ts": 1791799800,
            "duration_sec": 600,
            "tags": [
                {
                    "mac": "AA:BB:CC:DD:EE:FF",
                    "name": "freezer",
                    "message_id": "AA:BB:CC:DD:EE:FF-adv-1791799200",
                    "window_start": 1791799200,
                    "window_s": 600,
                    "count": 60,
                    "alarm_state": "WARNING",
                    "fields": {
                        "temperature": { "min": -19.5, "avg": -18.25, "max": -17.0 },
                        "humidity": { "min": 40.0, "avg": 41.5, "max": 43.0 },
                        "rssi": { "min": -80.0, "avg": -71.2, "max": -65.0 }
                    }
                }
            ]
        }"#;
        let msg: EyeAggregatedMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg.duration_sec, Some(600));
        let t = &msg.tags[0];
        assert_eq!(
            t.message_id.as_deref(),
            Some("AA:BB:CC:DD:EE:FF-adv-1791799200")
        );
        assert_eq!(t.count, 60);
        assert_eq!(t.alarm_state, "WARNING");
        assert_eq!(t.avg("temperature"), Some(-18.25));
        assert_eq!(t.fields["temperature"].min, Some(-19.5));
        assert_eq!(t.avg("battery"), None);
    }

    #[test]
    fn tolerates_missing_optional_fields_and_unknown_keys() {
        let json = r#"{ "tags": [ { "mac": "AA:BB:CC:DD:EE:FF", "future": 1 } ] }"#;
        let msg: EyeAggregatedMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg.tags[0].count, 0);
        assert_eq!(msg.tags[0].alarm_state, "NORMAL");
        assert!(msg.tags[0].fields.is_empty());
        assert!(msg.tags[0].message_id.is_none());
    }
}
