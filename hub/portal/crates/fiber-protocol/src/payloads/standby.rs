//! `{prefix}/{host}/power/standby` (retained) and
//! `{prefix}/{host}/power/events/standby` (edge) — operator standby state.
//! mqtt.md "power/standby" section. Both topics share this payload and
//! handler (`handle_device_standby`, `handlers.py:986-1049`).
//!
//! `standby` is the one truly required field — the handler rejects the
//! message (logs a warning, no state update) if it's missing or not a
//! bool. Every other field is read with plain `data.get(...)` and never
//! validated, so all are `Option` here even though mqtt.md doesn't mark
//! `requested_by`/`vin_mv` nullable in its example.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StandbyMsg {
    pub timestamp: Option<String>,
    pub standby: bool,
    pub reason: Option<String>,
    /// Empty string means self-initiated (device-triggered) standby.
    pub requested_by: Option<String>,
    pub entered_at: Option<String>,
    pub vin_mv: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_spec_example() {
        let json = r#"{
            "timestamp": "2026-08-05T12:00:00Z",
            "standby": true,
            "reason": "operator power_off",
            "requested_by": "dr.jane",
            "entered_at": "2026-08-05T12:00:00Z",
            "vin_mv": 0
        }"#;
        let msg: StandbyMsg = serde_json::from_str(json).unwrap();
        assert!(msg.standby);
        assert_eq!(msg.requested_by.as_deref(), Some("dr.jane"));
    }

    #[test]
    fn self_initiated_has_empty_requested_by() {
        let json = r#"{"standby": false, "requested_by": ""}"#;
        let msg: StandbyMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg.requested_by.as_deref(), Some(""));
    }
}
