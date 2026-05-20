//! JSON envelopes for the export streams.
//!
//! Each stream (`sticker`, `probe`, `alarm`) is published with a small
//! wrapper around the underlying DB row so downstream consumers have a
//! stable `message_id` (for dedup) and `stream` + `stream_version` (for
//! schema evolution). Topic shape: `export/{stream}/{key}` where `key` is
//! the natural per-record identifier (dev_eui, sensor line, or `sys`).

use crate::libs::storage::models::{AlarmEvent, SensorReading, StickerReadingRow};

pub fn sticker_envelope(row: &StickerReadingRow) -> (String, String) {
    let topic = format!("export/sticker/{}", row.dev_eui);
    let payload = serde_json::to_string(&serde_json::json!({
        "message_id":     row.message_id,
        "exported_at":    now_secs(),
        "stream":         "sticker",
        "stream_version": 1,
        "data": {
            "dev_eui":            row.dev_eui,
            "provisioning_epoch": row.provisioning_epoch,
            "ts":                 row.ts,
            "received_at":        row.received_at,
            "event_type":         row.event_type,
            "payload":            serde_json::from_str::<serde_json::Value>(&row.payload_json)
                                  .unwrap_or(serde_json::json!({})),
        }
    }))
    .unwrap_or_else(|_| "{}".to_string());
    (topic, payload)
}

pub fn probe_envelope(row: &SensorReading) -> (String, String) {
    let topic = format!("export/probe/{}", row.sensor_line);
    let payload = serde_json::to_string(&serde_json::json!({
        "message_id":     format!("probe-{}-{}", row.sensor_line, row.timestamp),
        "exported_at":    now_secs(),
        "stream":         "probe",
        "stream_version": 1,
        "data":           row,
    }))
    .unwrap_or_else(|_| "{}".to_string());
    (topic, payload)
}

pub fn alarm_envelope(row: &AlarmEvent) -> (String, String) {
    let topic = match row.sensor_line {
        u8::MAX => "export/alarm/sys".to_string(),
        n => format!("export/alarm/{}", n),
    };
    let payload = serde_json::to_string(&serde_json::json!({
        "message_id":     format!("alarm-{}-{}-{}", row.sensor_line, row.timestamp, row.id),
        "exported_at":    now_secs(),
        "stream":         "alarm",
        "stream_version": 1,
        "data":           row,
    }))
    .unwrap_or_else(|_| "{}".to_string());
    (topic, payload)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::storage::models::StickerReadingRow;

    #[test]
    fn sticker_envelope_includes_required_fields() {
        let row = StickerReadingRow {
            id: 7,
            dev_eui: "abc".into(),
            provisioning_epoch: 2,
            ts: 1000,
            received_at: 1001,
            message_id: "abc-1000-3".into(),
            event_type: "uplink".into(),
            payload_json: r#"{"fields":{"t":21}}"#.into(),
            created_at: 1001,
        };
        let (topic, payload) = sticker_envelope(&row);
        assert_eq!(topic, "export/sticker/abc");
        assert!(payload.contains("\"message_id\":\"abc-1000-3\""));
        assert!(payload.contains("\"stream\":\"sticker\""));
        assert!(payload.contains("\"provisioning_epoch\":2"));
    }

    #[test]
    fn probe_envelope_uses_sensor_line_in_topic_and_message_id() {
        let row = SensorReading {
            id: 11,
            timestamp: 1500,
            sensor_line: 3,
            temperature_c: 21.5,
            is_connected: true,
            alarm_state: "NORMAL".to_string(),
            created_at: 1500,
            data_hmac: None,
        };
        let (topic, payload) = probe_envelope(&row);
        assert_eq!(topic, "export/probe/3");
        assert!(payload.contains("\"message_id\":\"probe-3-1500\""));
        assert!(payload.contains("\"stream\":\"probe\""));
    }

    #[test]
    fn alarm_envelope_uses_sys_for_max_line() {
        let row = AlarmEvent {
            id: 5,
            timestamp: 2000,
            sensor_line: u8::MAX,
            from_state: "NORMAL".into(),
            to_state: "CRITICAL".into(),
            temperature_c: None,
            details: None,
        };
        let (topic, _payload) = alarm_envelope(&row);
        assert_eq!(topic, "export/alarm/sys");
    }
}
