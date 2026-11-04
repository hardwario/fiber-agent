//! `{prefix}/{host}/responses/{sensor_config,interval_config,history}` —
//! only these three sub-topics under `responses/#` are handled; mqtt.md
//! "responses/sensor_config", "responses/interval_config",
//! "responses/history" sections.

use serde::{Deserialize, Serialize};

use crate::payloads::config_state::{Intervals, SensorConfigEntry};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorConfigResponse {
    pub timestamp: Option<String>,
    #[serde(default)]
    pub sensors: Vec<SensorConfigEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntervalConfigResponse {
    pub intervals: Option<Intervals>,
}

/// Log-only in v1 (`handlers.py`): no state/WS side effect, kept here for
/// completeness of the `responses/#` family.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryResponse {
    pub request_id: Option<String>,
    pub status: Option<String>,
    pub rows_sent: Option<i64>,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensor_config_response_deserializes() {
        let json = r#"{"timestamp": "t", "sensors": [{"line": 1, "enabled": true, "has_override": true, "thresholds": {}}]}"#;
        let msg: SensorConfigResponse = serde_json::from_str(json).unwrap();
        assert_eq!(msg.sensors.len(), 1);
        assert_eq!(msg.sensors[0].line, 1);
    }

    #[test]
    fn interval_config_response_deserializes() {
        let json = r#"{"intervals": {"sample_interval_ms": 1000, "aggregation_interval_ms": null, "report_interval_ms": null}}"#;
        let msg: IntervalConfigResponse = serde_json::from_str(json).unwrap();
        assert_eq!(msg.intervals.unwrap().sample_interval_ms, Some(1000));
    }

    #[test]
    fn history_response_deserializes() {
        let json = r#"{"request_id": "r1", "status": "complete", "rows_sent": 42, "error": null}"#;
        let msg: HistoryResponse = serde_json::from_str(json).unwrap();
        assert_eq!(msg.rows_sent, Some(42));
    }
}
