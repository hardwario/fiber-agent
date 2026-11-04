//! `{prefix}/{host}/status` — device LWT.
//!
//! mqtt.md: `{"status": "online"|"offline", "timestamp": str}` — plain-text
//! `online`/`offline` also accepted, and an **empty payload reads as
//! offline**. Cross-checked against `handle_device_status`
//! (`handlers.py:1052-1089`): the JSON branch reads `status` with
//! `data.get('status', 'offline')` (so a JSON object missing `status`
//! defaults to offline, hence `Option` here rather than required), and the
//! exception branch lowercases the raw payload and compares to `'online'`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceStatus {
    Online,
    Offline,
}

/// The JSON-encoded form of the status payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusMsg {
    pub status: Option<String>,
    pub timestamp: Option<String>,
}

/// Parse a `status` topic payload, covering all three v1 encodings: a JSON
/// object, plain-text `online`/`offline`, and an empty payload (offline).
pub fn parse_status(bytes: &[u8]) -> DeviceStatus {
    if bytes.is_empty() {
        return DeviceStatus::Offline;
    }
    if let Ok(msg) = serde_json::from_slice::<StatusMsg>(bytes) {
        return match msg.status.as_deref() {
            Some(s) if s.eq_ignore_ascii_case("online") => DeviceStatus::Online,
            _ => DeviceStatus::Offline,
        };
    }
    let text = String::from_utf8_lossy(bytes);
    if text.trim().eq_ignore_ascii_case("online") {
        DeviceStatus::Online
    } else {
        DeviceStatus::Offline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_online() {
        assert_eq!(
            parse_status(br#"{"status":"online","timestamp":"2026-01-13T14:30:00Z"}"#),
            DeviceStatus::Online
        );
    }

    #[test]
    fn json_offline() {
        assert_eq!(
            parse_status(br#"{"status":"offline","timestamp":"2026-01-13T14:30:00Z"}"#),
            DeviceStatus::Offline
        );
    }

    #[test]
    fn plain_text_online() {
        assert_eq!(parse_status(b"online"), DeviceStatus::Online);
    }

    #[test]
    fn plain_text_offline() {
        assert_eq!(parse_status(b"offline"), DeviceStatus::Offline);
    }

    #[test]
    fn empty_payload_is_offline() {
        assert_eq!(parse_status(b""), DeviceStatus::Offline);
    }

    #[test]
    fn json_missing_status_defaults_offline() {
        assert_eq!(parse_status(br#"{"timestamp":"x"}"#), DeviceStatus::Offline);
    }
}
