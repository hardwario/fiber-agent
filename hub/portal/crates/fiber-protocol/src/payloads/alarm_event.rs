//! `{prefix}/{host}/alarms/events` — probe alarm transitions (not
//! retained). mqtt.md "alarms/events" section.
//!
//! Cross-checked against `handle_alarm_event` (`handlers.py:1091-1310`):
//! `line` defaults to `0`, `from_state`/`to_state` default to `"NORMAL"`
//! when absent (`event_data.get('from_state', 'NORMAL')`), kept as plain
//! strings rather than an enum per the task brief (firmware sends bare
//! `WARNING`/`CRITICAL` without a side, and `NEVERCONNECTED` etc. — modeling
//! this as a closed Rust enum would make an unrecognized-but-valid state
//! string a hard deserialize error). `alarm_type` is the one closed enum,
//! limited to the three connectivity variants that `handlers.py:1140`
//! checks explicitly; anything else is `Unknown` rather than a parse
//! failure, matching the crate-wide no-`deny_unknown_fields` leniency.

use serde::{Deserialize, Serialize};

fn default_normal() -> String {
    "NORMAL".to_string()
}

/// The three connectivity alarm types the firmware may report explicitly
/// (mqtt.md: `"alarm_type": "POWER_DISCONNECT|WIFI_DISCONNECT|ETHERNET_DISCONNECT"|null`).
/// `Unknown` absorbs any other value so an unrecognized string never fails
/// deserialization of the whole message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlarmType {
    #[serde(rename = "POWER_DISCONNECT")]
    PowerDisconnect,
    #[serde(rename = "WIFI_DISCONNECT")]
    WifiDisconnect,
    #[serde(rename = "ETHERNET_DISCONNECT")]
    EthernetDisconnect,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlarmEventMsg {
    pub timestamp: Option<String>,
    #[serde(default)]
    pub line: i64,
    pub name: Option<String>,
    #[serde(default = "default_normal")]
    pub from_state: String,
    #[serde(default = "default_normal")]
    pub to_state: String,
    pub temperature_celsius: Option<f64>,
    pub event_type: Option<String>,
    pub message: Option<String>,
    pub alarm_type: Option<AlarmType>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_spec_example() {
        let json = r#"{
            "timestamp": "2026-01-13T14:30:00Z",
            "line": 0,
            "name": "Sensor 1",
            "from_state": "NORMAL",
            "to_state": "WARNING",
            "temperature_celsius": 39.5,
            "event_type": "alarm_transition",
            "message": null,
            "alarm_type": null
        }"#;
        let msg: AlarmEventMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg.line, 0);
        assert_eq!(msg.to_state, "WARNING");
        assert_eq!(msg.temperature_celsius, Some(39.5));
        assert!(msg.alarm_type.is_none());
    }

    #[test]
    fn connectivity_alarm_type_parses() {
        let json = r#"{"line": 0, "to_state": "CRITICAL", "alarm_type": "POWER_DISCONNECT"}"#;
        let msg: AlarmEventMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg.alarm_type, Some(AlarmType::PowerDisconnect));
    }

    #[test]
    fn unknown_alarm_type_does_not_fail_parse() {
        let json = r#"{"line": 0, "to_state": "CRITICAL", "alarm_type": "SOMETHING_NEW"}"#;
        let msg: AlarmEventMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg.alarm_type, Some(AlarmType::Unknown));
    }

    #[test]
    fn missing_states_default_to_normal() {
        let msg: AlarmEventMsg = serde_json::from_str("{}").unwrap();
        assert_eq!(msg.from_state, "NORMAL");
        assert_eq!(msg.to_state, "NORMAL");
        assert_eq!(msg.line, 0);
    }
}
