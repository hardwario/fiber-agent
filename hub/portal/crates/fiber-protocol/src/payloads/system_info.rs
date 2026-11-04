//! `{prefix}/{host}/system/info` — retained, QoS 1, ~60s. The only source
//! of power/network/storage/LoRaWAN-gateway state. mqtt.md "system/info"
//! section explicitly notes "Any leaf may be null", so every field here is
//! `Option`.
//!
//! Cross-checked against `handle_system_info` (`handlers.py:793-984`): the
//! `_section()` helper (lines 862-877) treats every nested object as
//! optional, defaulting to `{}` when missing, `null`, or a non-object —
//! hence the nested containers (`power`, `battery`, `dc`, `network`,
//! `wifi`, `ethernet`, `storage`, `data_partition`, `lorawan`) are all
//! `Option` too, not just their leaves.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SystemInfoMsg {
    pub timestamp: Option<String>,
    pub device_label: Option<String>,
    pub firmware_version: Option<String>,
    pub uptime_seconds: Option<i64>,
    pub uptime_human: Option<String>,
    pub power: Option<Power>,
    pub network: Option<Network>,
    pub storage: Option<Storage>,
    pub lorawan: Option<LorawanGatewayInfo>,
    /// Whether an operator has finished setting this Hub up, and as what (GitLab #58).
    ///
    /// An **additive sibling key** — the house rule for evolving this payload: no
    /// `schema_version` bump, and no existing key's meaning widened. An agent that predates it
    /// simply omits the section, which reads here as `None` and must be rendered as "unknown"
    /// rather than as "not set up".
    pub setup: Option<SetupInfo>,
    /// The Hub's own hardware identity from its device tree (portal devices.hub.md §3.1,
    /// amendment 2026-09-28). Additive like `setup`: an agent that predates it, or one that
    /// could not read the device tree, omits it, and the portal shows "—". Skipped when `None`
    /// so a message without it serializes exactly as it did before the key existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware: Option<Hardware>,
    /// The Hub's Southbridge firmware and its update (plan/system/southbridge_update.md §3.5,
    /// amended 2026-10-09). Additive like `hardware`: absent on an agent that predates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub southbridge: Option<Southbridge>,
}

/// `system/info.southbridge` — what the Southbridge runs, what the image bundles, and how the
/// last update went.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Southbridge {
    /// The SB's `VERSION` answer without the prefix, e.g. "south-zephyr 1.1.0 g73f39ed5";
    /// absent when the SB did not answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The same string for the image this Hub bundles; absent when it bundles none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundled: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<SouthbridgeUpdate>,
}

/// `system/info.southbridge.update`, from `/data/fiber/southbridge/update-state.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SouthbridgeUpdate {
    /// `idle`, `running` or `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Failed attempts at start in this boot (the budget is 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// `ok <version>` or `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// `system/info.hardware` — what the compute module reports about itself.
///
/// Every field is absent rather than null when the Hub could not read it, and each one is
/// optional on its own: a device tree with a serial but no model is a real (if unlikely)
/// answer, not a malformed message. There is no HARDWARIO product EEPROM on the board, so
/// the Hub board's own revision is not here at all; `revision` is the compute module's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Hardware {
    /// The compute module's model with its revision suffix split off
    /// (`/proc/device-tree/model`, e.g. "Raspberry Pi Compute Module 4").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The compute module's revision, the part after " Rev " (e.g. "1.1"); absent when the
    /// model string carries none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// The full CPU serial (`/proc/device-tree/serial-number`, e.g. "100000004e970f4a"), not
    /// the eight characters the hostname carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
}

/// `system/info.setup` — the `setup:` section of the device's own config, republished so the
/// portal can show it without keeping a copy of its own.
///
/// This is deliberately a mirror, not a source: the value lives in `fiber.config.yaml`, is written
/// over BLE (FB17) or by the signed `setup-complete` command, and reaches the portal only here.
/// v1 kept the equivalent (`app_mode`) in the viewer's database, which went silent on exactly the
/// Hubs it mattered for — a client-role Hub shuts its portal down.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SetupInfo {
    pub provisioned: Option<bool>,
    /// `"server"` or `"client"`; absent until setup completes. Kept as a string rather than an
    /// enum for the same reason every other leaf here is `Option`: a newer agent naming a role
    /// this build does not know must not make the whole message unparseable.
    pub role: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Power {
    pub battery: Option<Battery>,
    pub dc: Option<Dc>,
    pub last_dc_loss: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Battery {
    pub voltage_mv: Option<i64>,
    pub percentage: Option<i64>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Dc {
    pub voltage_mv: Option<i64>,
    pub connected: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Network {
    pub wifi: Option<Wifi>,
    pub ethernet: Option<Ethernet>,
    pub has_internet: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Wifi {
    pub connected: Option<bool>,
    pub signal_dbm: Option<i64>,
    pub ip: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Ethernet {
    pub connected: Option<bool>,
    pub ip: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Storage {
    pub data_partition: Option<DataPartition>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DataPartition {
    pub total_bytes: Option<i64>,
    pub available_bytes: Option<i64>,
    pub used_percent: Option<f64>,
}

/// LoRaWAN gateway state carried in `system/info`. `cluster` is deliberately
/// untyped (`serde_json::Value`): mqtt.md notes it "never carries the peer
/// credential" on this retained topic but doesn't pin its shape, and no
/// v1 consumer reads it (LoRaWAN cluster UI is out of phase-1 scope per
/// design.md §2).
// `PartialEq`: the portal stores this block and emits a change event only when it actually
// changed (portal devices.hub.md §12 A) — `system/info` repeats every interval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LorawanGatewayInfo {
    /// Rendered `radio.mode` — `off`/`lorawan`/`p2p`/`dual`, or `None` on a Hub with no rendered
    /// radio config. Lets a consumer tell a missing concentrator that is a fault (`lorawan`/`dual`,
    /// where the Hub is supposed to have one) from one where its absence is normal (`off`/`p2p`).
    pub mode: Option<String>,
    pub gateway_present: Option<bool>,
    pub concentratord_running: Option<bool>,
    pub chirpstack_running: Option<bool>,
    pub sensor_count: Option<i64>,
    pub gateway_eui: Option<String>,
    pub cluster: Option<serde_json::Value>,
    /// The Hub's default JoinEUI (`radio.join_eui`), `None` without a v2 radio config or
    /// from an agent that predates it (#86). The portal prefills Node registration with it.
    #[serde(default)]
    pub join_eui: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_spec_example() {
        let json = r#"{
            "timestamp": "2026-01-13T14:30:00Z",
            "device_label": "Freezer 01",
            "firmware_version": "0.1.0",
            "uptime_seconds": 86400,
            "uptime_human": "1d 0h 0m 0s",
            "power": {
                "battery": { "voltage_mv": 4200, "percentage": 85, "status": "normal" },
                "dc": { "voltage_mv": 5000, "connected": true },
                "last_dc_loss": "2026-01-12T10:15:00Z"
            },
            "network": {
                "wifi": { "connected": true, "signal_dbm": -45, "ip": "192.168.1.100" },
                "ethernet": { "connected": false, "ip": null },
                "has_internet": true
            },
            "storage": {
                "data_partition": { "total_bytes": 5368709120, "available_bytes": 3221225472, "used_percent": 40 }
            },
            "lorawan": {
                "gateway_present": true, "concentratord_running": true,
                "chirpstack_running": true, "sensor_count": 2
            }
        }"#;
        let msg: SystemInfoMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg.device_label.as_deref(), Some("Freezer 01"));
        assert_eq!(
            msg.power
                .as_ref()
                .unwrap()
                .battery
                .as_ref()
                .unwrap()
                .percentage,
            Some(85)
        );
        assert_eq!(
            msg.network
                .as_ref()
                .unwrap()
                .ethernet
                .as_ref()
                .unwrap()
                .connected,
            Some(false)
        );
        assert_eq!(
            msg.storage
                .as_ref()
                .unwrap()
                .data_partition
                .as_ref()
                .unwrap()
                .used_percent,
            Some(40.0)
        );
        assert_eq!(msg.lorawan.as_ref().unwrap().sensor_count, Some(2));
    }

    #[test]
    fn every_leaf_may_be_null_or_absent() {
        let msg: SystemInfoMsg = serde_json::from_str("{}").unwrap();
        assert!(msg.power.is_none());
        assert!(msg.network.is_none());
        assert!(msg.storage.is_none());
        assert!(msg.lorawan.is_none());
        assert!(msg.hardware.is_none());
    }

    /// The `hardware` block as control-core's `libs::hardware` writes it for a CM4.
    const HARDWARE_JSON: &str = r#"{
        "firmware_version": "0.1.0",
        "lorawan": { "mode": "p2p" },
        "hardware": {
            "model": "Raspberry Pi Compute Module 4",
            "revision": "1.1",
            "serial": "100000004e970f4a"
        }
    }"#;

    #[test]
    fn hardware_round_trips() {
        let msg: SystemInfoMsg = serde_json::from_str(HARDWARE_JSON).unwrap();
        let hardware = msg.hardware.as_ref().expect("hardware parsed");
        assert_eq!(
            hardware.model.as_deref(),
            Some("Raspberry Pi Compute Module 4")
        );
        assert_eq!(hardware.revision.as_deref(), Some("1.1"));
        assert_eq!(hardware.serial.as_deref(), Some("100000004e970f4a"));
        assert_eq!(msg.lorawan.as_ref().unwrap().mode.as_deref(), Some("p2p"));

        let value = serde_json::to_value(&msg).unwrap();
        let back: SystemInfoMsg = serde_json::from_value(value).unwrap();
        assert_eq!(back.hardware, msg.hardware);
    }

    /// The `southbridge` block as control-core's `southbridge_update::status_json` writes it.
    #[test]
    fn southbridge_round_trips() {
        let msg: SystemInfoMsg = serde_json::from_str(
            r#"{ "southbridge": {
                "version": "south-zephyr 1.1.0 g73f39ed5",
                "bundled": "south-zephyr 1.1.0 g73f39ed5",
                "update": { "state": "idle", "attempts": 0, "last_result": "ok south-zephyr 1.1.0 g73f39ed5",
                            "updated_at": "2026-10-09T12:00:00Z" } } }"#,
        )
        .unwrap();
        let sb = msg.southbridge.as_ref().expect("southbridge parsed");
        assert_eq!(sb.version.as_deref(), Some("south-zephyr 1.1.0 g73f39ed5"));
        let up = sb.update.as_ref().unwrap();
        assert_eq!(up.state.as_deref(), Some("idle"));
        assert!(up.last_error.is_none());
        let back: SystemInfoMsg =
            serde_json::from_value(serde_json::to_value(&msg).unwrap()).unwrap();
        assert_eq!(back.southbridge, msg.southbridge);
        let none: SystemInfoMsg = serde_json::from_str("{}").unwrap();
        assert!(serde_json::to_value(&none)
            .unwrap()
            .get("southbridge")
            .is_none());
    }

    /// Each field is optional on its own, and an absent one stays absent on the way out.
    #[test]
    fn a_partial_hardware_block_parses_and_omits_the_rest() {
        let msg: SystemInfoMsg =
            serde_json::from_str(r#"{ "hardware": { "serial": "100000004e970f4a" } }"#).unwrap();
        let hardware = msg.hardware.as_ref().unwrap();
        assert_eq!(hardware.serial.as_deref(), Some("100000004e970f4a"));
        assert!(hardware.model.is_none());
        assert!(hardware.revision.is_none());

        let value = serde_json::to_value(&msg).unwrap();
        assert_eq!(
            value["hardware"],
            serde_json::json!({ "serial": "100000004e970f4a" })
        );
    }

    /// Additive: a payload from an agent that predates the key parses, and a message without
    /// it serializes with no `hardware` key at all rather than a `null` one.
    #[test]
    fn a_payload_without_hardware_is_unchanged() {
        let msg: SystemInfoMsg = serde_json::from_str(
            r#"{ "firmware_version": "0.1.0", "setup": { "provisioned": true } }"#,
        )
        .unwrap();
        assert!(msg.hardware.is_none());
        assert_eq!(msg.setup.as_ref().unwrap().provisioned, Some(true));

        let value = serde_json::to_value(&msg).unwrap();
        assert!(value.get("hardware").is_none(), "{value}");
    }
}
