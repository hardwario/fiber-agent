//! `{prefix}/{host}/errors` — device-reported command failures, dispatched
//! last so it can't shadow `responses/#`. mqtt.md "errors" section.
//!
//! Cross-checked against `handle_device_error` (`handlers.py:1617-1646`):
//! `command`/`error` fall back to `"unknown"` and `message` falls back to
//! `error` then a literal string when absent — all read via
//! `data.get(...) or ...`, so all fields are `Option` here; the crate
//! leaves the "unknown" defaulting to callers rather than baking a policy
//! string into the wire type.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceErrorMsg {
    pub timestamp: Option<String>,
    pub command: Option<String>,
    pub error: Option<String>,
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_spec_example() {
        let json = r#"{"timestamp": "t", "command": "set_display_lines", "error": "invalid_params", "message": "too many lines"}"#;
        let msg: DeviceErrorMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg.command.as_deref(), Some("set_display_lines"));
    }

    #[test]
    fn tolerates_all_fields_missing() {
        let msg: DeviceErrorMsg = serde_json::from_str("{}").unwrap();
        assert!(msg.command.is_none());
    }
}
