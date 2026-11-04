//! EYE (BLE beacon) payloads — mqtt.md's "eye/sensors" section.
//!
//! B5 (devices.backend.md §8 WP5.1, §20) removed the EN12830 half of this
//! module: `EyeDetectMsg` (the `eye/detect` result envelope) and
//! `EyeTag.is_en12830`. There is one kind of Beacon and there never will be
//! another (devices.beacon.md §1), so the type probe and the capability
//! flag it fed have no meaning left. The Hub still *publishes* both — it
//! keeps `detect_eye_tag` in mqtt.md's 44-entry `SIGNED_COMMANDS` table and
//! `control-core` still answers it — so this is the portal declining to
//! read a field, not a wire change; neither struct here denies unknown
//! fields, so the extra key deserializes away silently.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EyeSensorsMsg {
    pub timestamp: Option<String>,
    #[serde(default)]
    pub tags: Vec<EyeTag>,
    /// The Hub dropped overheard strangers from this snapshot — either
    /// because its own discovered set is at `discovered_max`, or because
    /// `build_eye_sensors_payload`'s cap ladder had to shed live strangers
    /// to fit the payload budget (`control.ble.md` §5, Part B **UI-12**).
    ///
    /// `#[serde(default)]` because a Hub older than B6 does not send it, and
    /// a missing flag has to read as "nothing was dropped" rather than
    /// failing the whole snapshot. **Never true for `tags: []`** — an empty
    /// array is the empty snapshot, not a cap.
    ///
    /// Load-bearing for reconcile-by-absence: the portal prunes a
    /// discovered-only beacon the moment it leaves the array, so a capped
    /// snapshot would otherwise empty the scan table on one oversized
    /// publish (devices.backend.md §22.1).
    #[serde(default)]
    pub discovered_capped: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EyeTag {
    pub gateway: Option<String>,
    /// The portal uppercases this on ingest; wire case is not guaranteed.
    pub mac: String,
    pub name: Option<String>,
    pub temperature_c: Option<f64>,
    pub humidity_pct: Option<i64>,
    pub battery_mv: Option<i64>,
    pub low_battery: Option<bool>,
    pub magnet_present: Option<bool>,
    pub magnet_detected: Option<bool>,
    pub moving: Option<bool>,
    pub movement_count: Option<i64>,
    pub pitch_deg: Option<i64>,
    pub roll_deg: Option<i64>,
    pub rssi: Option<i64>,
    /// Unix seconds.
    pub last_seen_ts: Option<i64>,
    pub stale: Option<bool>,
    pub provisioning: Option<String>,
    pub discovered: Option<bool>,
    #[serde(default)]
    pub field_alarm_states: HashMap<String, String>,
    pub alarm_state: Option<String>,
    /// The configuration the beacon reported the last time the Hub read it
    /// over GATT — after provisioning it, or right after it was enrolled
    /// (control/ble central.md §5, amended 2026-09-22).
    ///
    /// **Tri-state**, unlike every field above: absent = no news (the Hub's
    /// trimmed payload rungs omit the key), `null` = cleared (the Hub no
    /// longer vouches for it), a value = verified. [`double_option`] keeps the
    /// first two apart, which a plain `Option` would fold together.
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub ble_config: Option<Option<EyeBleConfig>>,
}

/// `eye/sensors` tag `ble_config` — what the beacon reported when the Hub read
/// its configuration characteristics back. Lenient like the rest of this
/// module: every field optional, sensor names kept as the Hub spelled them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EyeBleConfig {
    pub advertising_interval_ms: Option<i64>,
    pub tx_power_dbm: Option<i64>,
    pub protocol_type: Option<i64>,
    /// `protocol_type` by name — `eye_sensor`, `ibeacon`, … (control.ble.md,
    /// amended 2026-09-23). Kept as the Hub spelled it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// Every bit of the active-sensor mask, on or off; `null` on a recorder
    /// beacon, which has no such characteristic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensors: Option<BTreeMap<String, bool>>,
    /// The earlier spelling: only the sensors that were on. Read from a Hub
    /// that predates `sensors`; a current Hub does not send it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_sensors: Option<Vec<String>>,
    /// When it was read, Unix seconds.
    pub read_at: Option<i64>,
    /// `true` when the read closed a session that wrote the profile and it
    /// matched; `false` for a plain read, which vouches for nothing.
    pub verified: Option<bool>,
    /// Whether the Beacon's archive is recording, off its record info
    /// (beacon_history.md §8). Absent until the Hub has read it once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<bool>,
    /// The archive's cadence in minutes (1, 5 or 15), from the same read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging_interval_min: Option<i64>,
}

/// A present key — `null` included — deserializes as `Some(..)`; together with
/// `#[serde(default)]` an absent key stays `None`.
fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eye_sensors_deserializes_spec_example() {
        let json = r#"{
            "timestamp": "t",
            "discovered_capped": true,
            "tags": [ {
                "gateway": "gw1", "mac": "AA:BB:CC:DD:EE:FF", "name": "Tag 1",
                "temperature_c": 21.5, "humidity_pct": 45, "battery_mv": 3000,
                "low_battery": false, "magnet_present": null, "magnet_detected": null,
                "moving": false, "movement_count": 0, "pitch_deg": 1, "roll_deg": 2,
                "rssi": -70, "last_seen_ts": 1736775000, "stale": false,
                "provisioning": null, "discovered": true, "is_en12830": true,
                "field_alarm_states": {"temperature": "NORMAL"}, "alarm_state": "NORMAL"
            } ]
        }"#;
        let msg: EyeSensorsMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg.tags.len(), 1);
        assert_eq!(msg.tags[0].mac, "AA:BB:CC:DD:EE:FF");
        assert_eq!(msg.tags[0].temperature_c, Some(21.5));
        assert!(msg.discovered_capped, "B6: the cap flag is on the envelope");
    }

    /// A Hub older than B6 sends no `discovered_capped`, and the whole point
    /// of the flag is that its absence must read as "nothing was dropped" —
    /// reconcile-by-absence prunes a discovered-only beacon the moment it
    /// leaves the array, and a snapshot that failed to parse would be worse
    /// than one that under-reports a cap.
    #[test]
    fn a_snapshot_without_discovered_capped_reads_as_false() {
        let json = r#"{ "timestamp": "t", "tags": [] }"#;
        let msg: EyeSensorsMsg = serde_json::from_str(json).expect("the field is optional");
        assert!(!msg.discovered_capped);
        assert!(msg.tags.is_empty());
    }

    /// Pins the consumer side of the control-core degradation contract:
    /// `MqttPublisher`'s `eye/sensors` ladder can fall back to
    /// `beacon_tag_json_minimal`, which emits only these eleven keys. Every
    /// omitted field must land as `None`/default here, never as a parse error.
    #[test]
    fn a_minimal_degraded_tag_still_deserializes() {
        let json = r#"{
            "timestamp": "t",
            "tags": [ {
                "mac": "AA:BB:CC:DD:EE:FF", "name": "Tag 1",
                "temperature_c": 21.5, "humidity_pct": 45, "low_battery": false,
                "last_seen_ts": 1736775000, "stale": true,
                "provisioning": "provisioned", "discovered": true,
                "field_alarm_states": {"temperature": "NORMAL"}, "alarm_state": "NORMAL"
            } ]
        }"#;
        let msg: EyeSensorsMsg = serde_json::from_str(json).unwrap();
        let tag = &msg.tags[0];
        assert_eq!(tag.mac, "AA:BB:CC:DD:EE:FF");
        // The four fields the portal store assigns verbatim survive intact...
        assert_eq!(tag.stale, Some(true));
        assert_eq!(tag.discovered, Some(true));
        assert_eq!(tag.provisioning.as_deref(), Some("provisioned"));
        assert_eq!(
            tag.field_alarm_states
                .get("temperature")
                .map(String::as_str),
            Some("NORMAL")
        );
        assert_eq!(tag.alarm_state.as_deref(), Some("NORMAL"));
        // ...and the trimmed diagnostics degrade to None, which the store
        // treats as "keep last known" rather than a clobber.
        assert!(tag.gateway.is_none());
        assert!(tag.battery_mv.is_none());
        assert!(tag.rssi.is_none());
        assert!(tag.pitch_deg.is_none());
    }

    /// B5 (§20): the Hub still emits `is_en12830` on every tag — it is a
    /// field of `control-core`'s own publisher, not something the portal can
    /// switch off — so the removal is only safe as long as an unknown key
    /// stays a no-op here. This is the test that says so: the spec example
    /// above already carries the key, and this one proves a tag consisting
    /// of nothing else still parses rather than erroring on it.
    #[test]
    fn a_v2_beacon_carries_no_tag_type() {
        let json = r#"{
            "timestamp": "t",
            "tags": [ { "mac": "AA:BB:CC:DD:EE:FF", "is_en12830": true } ]
        }"#;
        let msg: EyeSensorsMsg = serde_json::from_str(json).expect("unknown keys are ignored");
        assert_eq!(msg.tags[0].mac, "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn ble_config_keeps_absent_null_and_value_apart() {
        let tag = |extra: &str| -> EyeTag {
            let json = format!(r#"{{ "mac": "AA:BB:CC:DD:EE:FF"{extra} }}"#);
            serde_json::from_str(&json).expect("tag")
        };
        assert_eq!(tag("").ble_config, None, "absent: no news");
        assert_eq!(
            tag(r#", "ble_config": null"#).ble_config,
            Some(None),
            "null: cleared"
        );
        let verified = tag(
            r#", "ble_config": {"advertising_interval_ms": 10000, "tx_power_dbm": -8,
                "protocol_type": 2, "active_sensors": ["temperature", "humidity"],
                "read_at": 1790106435, "verified": true}"#,
        );
        assert_eq!(
            verified.ble_config,
            Some(Some(EyeBleConfig {
                recording: None,
                logging_interval_min: None,
                advertising_interval_ms: Some(10_000),
                tx_power_dbm: Some(-8),
                protocol_type: Some(2),
                protocol: None,
                sensors: None,
                active_sensors: Some(vec!["temperature".into(), "humidity".into()]),
                read_at: Some(1_790_106_435),
                verified: Some(true),
            }))
        );
        let named = tag(
            r#", "ble_config": {"protocol_type": 2, "protocol": "eye_sensor",
                "sensors": {"temperature": true, "humidity": true, "magnet": false,
                "movement": false}}"#,
        )
        .ble_config
        .flatten()
        .expect("a value");
        assert_eq!(named.protocol.as_deref(), Some("eye_sensor"));
        assert_eq!(
            named.sensors.as_ref().and_then(|m| m.get("magnet")),
            Some(&false),
            "a sensor that is off is on the wire, not missing"
        );
        let recorder =
            tag(r#", "ble_config": {"advertising_interval_ms": 10000, "active_sensors": null}"#);
        assert_eq!(
            recorder.ble_config.flatten().and_then(|c| c.active_sensors),
            None
        );
    }
}

/// `{prefix}/{server}/eye/homes`, retained (decision #42): the cluster's Beacon registry and each
/// registered Beacon's home Hub, as the Server's agent computed them. The portal only displays
/// it and routes by it; it computes nothing. The producer is
/// `control_ble::central::cluster::HomesDoc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EyeHomesMsg {
    #[serde(default)]
    pub v: u32,
    /// The cluster Server: the registry, and the hostname every Beacon is keyed by.
    pub server: String,
    #[serde(default)]
    pub registered: Vec<String>,
    /// MAC → home Hub.
    #[serde(default)]
    pub homes: BTreeMap<String, String>,
}
