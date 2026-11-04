//! Node readings for the LCD on a v2 Hub (#180).
//!
//! The display draws Nodes from a [`SharedLoRaWANState`], which only the legacy
//! `LoRaWANMonitor` fills. Where `radio.mode` names a transport that monitor is off
//! ([`crate::libs::radio_gate`]) and Node uplinks are decoded by `proximos-control`, which
//! publishes them on `proximos/{hostname}/nodes/{dev_eui}/{reading|info}`
//! (`control_radio::northbound`). This module subscribes to this Hub's own Nodes there and
//! keeps a display-only copy of their last values. It feeds nothing else: no alarms, no
//! publishing, no storage.

use std::sync::OnceLock;

use control_radio::northbound::{parse_node_topic, Area, ROOT};

use crate::libs::lorawan::chirpstack::NodeReading;
use crate::libs::lorawan::{create_shared_lorawan_state, SharedLoRaWANState};

/// The display's Node list on a v2 Hub.
pub fn state() -> &'static SharedLoRaWANState {
    static S: OnceLock<SharedLoRaWANState> = OnceLock::new();
    S.get_or_init(|| create_shared_lorawan_state(false))
}

/// The topics this Hub's MQTT monitor subscribes to for its own Nodes.
pub fn subscriptions(hostname: &str) -> Vec<String> {
    [Area::Reading, Area::Info]
        .into_iter()
        .map(|a| format!("{ROOT}/{hostname}/nodes/+/{}", a.as_str()))
        .collect()
}

/// An incoming publish. `true` when it was one of ours and needs nothing else.
pub fn on_publish(hostname: &str, topic: &str, payload: &[u8]) -> bool {
    let Some(t) = parse_node_topic(topic) else {
        return false;
    };
    if !t.hostname.eq_ignore_ascii_case(hostname) {
        return false;
    }
    let dev_eui = t.id.to_ascii_lowercase();
    match t.area {
        Area::Reading => {
            if let Ok(r) =
                serde_json::from_slice::<control_radio::decode::reading::NodeReading>(payload)
            {
                apply_reading(&dev_eui, &r);
            }
            true
        }
        Area::Info => {
            apply_info(&dev_eui, payload);
            true
        }
        _ => false,
    }
}

fn apply_reading(dev_eui: &str, r: &control_radio::decode::reading::NodeReading) {
    let reading = NodeReading {
        dev_eui: dev_eui.to_string(),
        device_name: String::new(),
        fields: r.fields.iter().map(|(k, v)| (k.clone(), *v)).collect(),
        counters: r.counters.iter().map(|(k, v)| (k.clone(), *v)).collect(),
        events: Vec::new(),
        gateways: Vec::new(),
        dr: r.dr.map(i64::from),
        rssi: r.rssi,
        snr: r.snr.map(|s| s as f32),
        received_at: r.received_at.clone(),
        fport: None,
        page: None,
    };
    let Ok(mut s) = state().write() else {
        return;
    };
    s.update_sensor(&reading);
    if let Some(sensor) = s.sensors.get_mut(dev_eui) {
        if sensor.serial_number.is_none() {
            sensor.serial_number = r.serial.clone();
        }
        if sensor.name.is_empty() {
            if let Some(serial) = &r.serial {
                sensor.name = serial.clone();
            }
        }
    }
}

/// The retained `info` document: the Node's name and serial, or an empty payload when the
/// Node was removed.
fn apply_info(dev_eui: &str, payload: &[u8]) {
    let Ok(mut s) = state().write() else {
        return;
    };
    if payload.is_empty() {
        s.sensors.remove(dev_eui);
        return;
    }
    let Ok(doc) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return;
    };
    let text = |k: &str| {
        doc.get(k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    if let Some(sensor) = s.sensors.get_mut(dev_eui) {
        if let Some(serial) = text("serial") {
            sensor.serial_number = Some(serial);
        }
        if let Some(name) = text("name").or_else(|| sensor.serial_number.clone()) {
            sensor.name = name;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "proximos-0a1b2c3d";
    const EUI: &str = "70b3d5e050011234";

    #[test]
    fn a_reading_then_its_info_give_the_display_a_named_node() {
        let reading = serde_json::json!({
            "message_id": "m1", "dev_eui": EUI, "serial": "S123",
            "fields": {"temperature": 4.5, "humidity": 51.0},
            "counters": {}, "events": [], "gateways": [],
            "received_at": "2026-10-30T10:00:00Z"
        });
        let topic = |a: &str| format!("proximos/{HOST}/nodes/{EUI}/{a}");
        assert!(on_publish(
            HOST,
            &topic("reading"),
            reading.to_string().as_bytes()
        ));
        assert!(on_publish(
            HOST,
            &topic("info"),
            br#"{"name":"Fridge 2","serial":"S123"}"#
        ));
        // Another Hub's Node is not ours to show.
        assert!(!on_publish(
            HOST,
            &format!("proximos/other/nodes/{EUI}/reading"),
            b"{}"
        ));

        let s = state().read().unwrap();
        let n = &s.sensors[EUI];
        assert_eq!(n.fields["humidity"], 51.0);
        assert_eq!(n.name, "Fridge 2");
        assert_eq!(n.serial_number.as_deref(), Some("S123"));
        drop(s);

        assert!(on_publish(HOST, &topic("info"), b""));
        assert!(!state().read().unwrap().sensors.contains_key(EUI));
    }
}
