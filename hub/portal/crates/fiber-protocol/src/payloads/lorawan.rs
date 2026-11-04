//! LoRaWAN Node (STICKER) payloads — mqtt.md "lorawan/sensors" and
//! `.../lorawan/sensors/{dev_eui}/{config,full-config,info,command,history}`
//! sections, plus `lorawan/gateways`. Out of phase-1 scope (design.md §2
//! lists Nodes as in-scope but the M0 crate only needs the wire types;
//! M5 wires up the handlers). Where mqtt.md uses "…" to elide a repetitive
//! key list (Node `info`'s exact field set), this module keeps the
//! documented fields typed and captures the rest via `#[serde(flatten)]`
//! into a generic map rather than guessing field names that aren't pinned
//! anywhere in the inventory — flagged in the M0-B report as a known gap
//! for M5 to firm up against a real device capture.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LorawanSensorsMsg {
    pub timestamp: Option<String>,
    #[serde(default)]
    pub sensors: Vec<LorawanSensor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LorawanSensor {
    /// Lowercased hex EUI.
    pub dev_eui: String,
    pub name: Option<String>,
    pub serial_number: Option<String>,
    pub location: Option<String>,
    /// Field registry values (`app/lorawan/field_registry.py`); v1 drops
    /// payloads missing this key entirely.
    #[serde(default)]
    pub fields: HashMap<String, f64>,
    #[serde(default)]
    pub field_alarm_states: HashMap<String, String>,
    /// Present as a list ⇒ authoritative for this Node (see
    /// `handlers.py:2610`, `thresholds_authoritative`).
    pub field_thresholds: Option<Vec<FieldThreshold>>,
    #[serde(default)]
    pub counters: HashMap<String, i64>,
    #[serde(default)]
    pub events: Vec<LorawanEvent>,
    pub rssi: Option<i64>,
    pub snr: Option<f64>,
    #[serde(default)]
    pub gateways: Vec<LorawanGatewayRssi>,
    pub fcnt: Option<i64>,
    pub dr: Option<i64>,
    pub downlink_gateway_id: Option<String>,
    /// Node uplink time — persistence dedup key.
    pub last_seen: Option<String>,
    pub alarm_state: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldThreshold {
    pub field: String,
    /// `null` means "inherit" (mqtt.md `set_lorawan_field_threshold`).
    pub critical_low: Option<f64>,
    pub warning_low: Option<f64>,
    pub warning_high: Option<f64>,
    pub critical_high: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LorawanGatewayRssi {
    pub gateway_id: Option<String>,
    pub rssi: Option<i64>,
    pub snr: Option<f64>,
}

/// A Node event. `event_type` and `ts` are the only fields every event
/// shares; the native fPort-3 alarm decode
/// (`{slot, source, quantity, edge, alarm_type|side, value, base_time, rel_s}`)
/// and any other event-specific keys land in `extra`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LorawanEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub ts: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LorawanGatewaysMsg {
    pub timestamp: Option<String>,
    #[serde(default)]
    pub gateways: Vec<ExternalGateway>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalGateway {
    pub gateway_eui: String,
    pub name: Option<String>,
    pub online: Option<bool>,
    pub last_seen: Option<String>,
}

/// `.../lorawan/sensors/{dev_eui}/config` — Node device-config pages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfigMsg {
    pub dev_eui: Option<String>,
    #[serde(default)]
    pub config: serde_json::Map<String, serde_json::Value>,
    pub page_index: Option<i64>,
    pub page_count: Option<i64>,
    pub synced_at: Option<String>,
    pub last_ack: Option<NodeLastAck>,
    pub read_status: Option<String>,
    pub missing: Option<Vec<String>>,
    pub error: Option<String>,
    pub chunks_done: Option<i64>,
    pub chunks_total: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeLastAck {
    pub seq: Option<i64>,
    pub result: Option<String>,
}

/// `.../lorawan/sensors/{dev_eui}/full-config` — same shape as
/// `NodeConfigMsg` plus `last_seq`; never contains secret keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeFullConfigMsg {
    pub dev_eui: Option<String>,
    #[serde(default)]
    pub config: serde_json::Map<String, serde_json::Value>,
    pub page_index: Option<i64>,
    pub page_count: Option<i64>,
    pub synced_at: Option<String>,
    pub last_ack: Option<NodeLastAck>,
    pub read_status: Option<String>,
    pub missing: Option<Vec<String>>,
    pub error: Option<String>,
    pub chunks_done: Option<i64>,
    pub chunks_total: Option<i64>,
    pub last_seq: Option<i64>,
}

/// `.../lorawan/sensors/{dev_eui}/info` — retained, projected info object.
/// An empty payload is a tombstone for a removed Node (check payload
/// length before deserializing, same as v1). Field set beyond what
/// mqtt.md spells out (`dev_eui`, `active_alarms`, `seq`, `source`,
/// `has_claim_token`) is not pinned in the inventory (elided with "fw
/// version" / "…" prose rather than a JSON key) — kept in `extra` until
/// M5 has a real capture to check against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfoMsg {
    pub dev_eui: Option<String>,
    pub active_alarms: Option<serde_json::Value>,
    pub seq: Option<i64>,
    /// `"unsolicited"` on join.
    pub source: Option<String>,
    /// `claim_token` is reduced to this boolean on the wire — never the
    /// secret itself.
    pub has_claim_token: Option<bool>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// `.../lorawan/sensors/{dev_eui}/command` — Node command results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeCommandResultMsg {
    pub dev_eui: Option<String>,
    pub command: Option<String>,
    pub seq: Option<i64>,
    /// May be `"requested"` — command went out but this one has no ack.
    pub result: Option<String>,
    pub expect: Option<String>,
    pub detail: Option<String>,
    pub fault_key: Option<String>,
    pub ts: Option<String>,
}

/// `.../lorawan/sensors/{dev_eui}/history` — ring-buffer replay frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeHistoryMsg {
    pub dev_eui: Option<String>,
    pub frame_index: Option<i64>,
    pub frame_count: Option<i64>,
    /// Seconds between records. The v2 radio publishes it; v1 frames leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_s: Option<i64>,
    #[serde(default)]
    pub records: Vec<NodeHistoryRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeHistoryRecord {
    /// Unix seconds.
    pub time: Option<i64>,
    #[serde(default)]
    pub fields: HashMap<String, Option<f64>>,
    #[serde(default)]
    pub counters: HashMap<String, Option<i64>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lorawan_sensors_deserializes_spec_example() {
        let json = r#"{
            "timestamp": "t",
            "sensors": [ {
                "dev_eui": "0011223344556677", "name": "Node 1", "serial_number": "SN1",
                "location": "Room A",
                "fields": { "temperature": 21.5 },
                "field_alarm_states": { "temperature": "NORMAL" },
                "field_thresholds": [ {"field": "temperature", "critical_low": null, "warning_low": -5.0, "warning_high": 30.0, "critical_high": 40.0} ],
                "counters": { "fCnt": 12 },
                "events": [ {"type": "alarm", "ts": "t", "slot": 1, "value": 41.2} ],
                "rssi": -80, "snr": 7.5,
                "gateways": [ {"gateway_id": "gw1", "rssi": -80, "snr": 7.5} ],
                "fcnt": 12, "dr": 5, "downlink_gateway_id": "gw1",
                "last_seen": "2026-01-13T14:30:00Z", "alarm_state": "NORMAL"
            } ]
        }"#;
        let msg: LorawanSensorsMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg.sensors.len(), 1);
        let s = &msg.sensors[0];
        assert_eq!(s.fields.get("temperature"), Some(&21.5));
        assert_eq!(
            s.field_thresholds.as_ref().unwrap()[0].warning_high,
            Some(30.0)
        );
        assert_eq!(s.events[0].event_type, "alarm");
        assert_eq!(
            s.events[0].extra.get("slot").and_then(|v| v.as_i64()),
            Some(1)
        );
    }

    #[test]
    fn node_history_deserializes() {
        let json = r#"{"dev_eui": "aa", "frame_index": 0, "frame_count": 1,
            "records": [ {"time": 1736775000, "fields": {"temperature": 21.5, "humidity": null}, "counters": {"fCnt": 1}} ]}"#;
        let msg: NodeHistoryMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg.records.len(), 1);
        assert_eq!(msg.records[0].fields.get("humidity"), Some(&None));
    }

    #[test]
    fn node_info_tolerates_unknown_fields() {
        let json = r#"{"dev_eui": "aa", "seq": 3, "source": "unsolicited", "fw_version": "1.2.3", "uptime_s": 100}"#;
        let msg: NodeInfoMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg.seq, Some(3));
        assert_eq!(
            msg.extra.get("fw_version").and_then(|v| v.as_str()),
            Some("1.2.3")
        );
    }

    /// Regression guard for `serde_json`'s `arbitrary_precision` feature.
    ///
    /// `control-core` enables `serde_json/arbitrary_precision`; under resolver
    /// 2 that unifies into this crate for any single-invocation workspace
    /// build. With the feature on, JSON numbers deserialize through an
    /// internal one-key map (`$serde_json::private::Number`), which is exactly
    /// what historically broke numbers travelling through `#[serde(flatten)]`
    /// (serde_json #721). `node_info_tolerates_unknown_fields` above only
    /// covers integers; this pins `seq: Option<i64>` next to a *float* landing
    /// in the flattened `extra` catch-all, from real JSON text, and pins that
    /// re-serialization stays flat and marker-free. Must pass with the feature
    /// on and off.
    #[test]
    fn node_info_deserializes_floats_through_flatten_under_arbitrary_precision() {
        let json = r#"{
            "dev_eui": "0011223344556677", "seq": 42, "source": "unsolicited",
            "has_claim_token": true,
            "active_alarms": {"temperature": "WARNING_HIGH"},
            "battery_v": 3.62, "temperature_c": -0.5, "uptime_s": 100,
            "fw_version": "1.2.3"
        }"#;
        let msg: NodeInfoMsg = serde_json::from_str(json).unwrap();

        assert_eq!(msg.seq, Some(42));
        assert_eq!(msg.dev_eui.as_deref(), Some("0011223344556677"));
        assert_eq!(msg.has_claim_token, Some(true));
        assert_eq!(
            msg.extra.get("battery_v").and_then(|v| v.as_f64()),
            Some(3.62)
        );
        assert_eq!(
            msg.extra.get("temperature_c").and_then(|v| v.as_f64()),
            Some(-0.5)
        );
        assert_eq!(
            msg.extra.get("uptime_s").and_then(|v| v.as_i64()),
            Some(100)
        );

        let out = serde_json::to_string(&msg).unwrap();
        assert!(
            !out.contains("$serde_json::private::Number"),
            "arbitrary_precision leaked its private number marker: {out}"
        );
        let reparsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(reparsed["seq"].as_i64(), Some(42));
        assert_eq!(reparsed["battery_v"].as_f64(), Some(3.62));
        assert_eq!(reparsed["temperature_c"].as_f64(), Some(-0.5));
    }

    /// Companion to the test above for the other payload whose numbers ride
    /// inside a `serde_json::Map` rather than typed fields: a device config
    /// page. Same `arbitrary_precision` concern — the map values are `Value`s
    /// built by the number deserializer — plus `Option<i64>` page counters and
    /// a nested `seq`.
    #[test]
    fn node_config_deserializes_numeric_config_values_under_arbitrary_precision() {
        let json = r#"{
            "dev_eui": "0011223344556677",
            "config": {
                "warning_high": 30.5, "critical_high": 40.0,
                "warning_low": -5.25, "interval_s": 900,
                "label": "Room A"
            },
            "page_index": 0, "page_count": 3,
            "synced_at": "2026-01-13T14:30:00Z",
            "last_ack": {"seq": 7, "result": "ok"},
            "read_status": "complete",
            "chunks_done": 3, "chunks_total": 3
        }"#;
        let msg: NodeConfigMsg = serde_json::from_str(json).unwrap();

        assert_eq!(msg.page_index, Some(0));
        assert_eq!(msg.page_count, Some(3));
        assert_eq!(msg.chunks_done, Some(3));
        assert_eq!(msg.last_ack.as_ref().unwrap().seq, Some(7));
        assert_eq!(
            msg.config.get("warning_high").and_then(|v| v.as_f64()),
            Some(30.5)
        );
        assert_eq!(
            msg.config.get("warning_low").and_then(|v| v.as_f64()),
            Some(-5.25)
        );
        assert_eq!(
            msg.config.get("interval_s").and_then(|v| v.as_i64()),
            Some(900)
        );

        let out = serde_json::to_string(&msg).unwrap();
        assert!(
            !out.contains("$serde_json::private::Number"),
            "arbitrary_precision leaked its private number marker: {out}"
        );
        let reparsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(reparsed["config"]["warning_high"].as_f64(), Some(30.5));
        assert_eq!(reparsed["last_ack"]["seq"].as_i64(), Some(7));
    }
}
