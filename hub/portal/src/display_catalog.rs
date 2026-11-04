//! The Unit tab's sensor picker catalogue (plan/portal/display-config.md
//! §5, WP-B/B2 + B4) — what may go on the Hub's LCD, and how a checked box
//! turns back into a [`fiber_protocol::DisplayLine`].
//!
//! One list built from `PortalState` alone, no agent round-trip: the eight
//! wired lines of this Hub, then its Nodes, then its registered Beacons —
//! the order the built-in layout uses. Each entry carries a stable opaque
//! `id` (`ds18b20:3`, `sticker:<dev_eui>:<field>`, `ble:<MAC>:<field>`); the frontend
//! echoes that id verbatim and never sees a `DisplayLine`, which is what
//! keeps the addressing (`line`/`dev_eui`/`mac`) backend-private.
//!
//! `selected` is decided by **address**, never by position in the list: a
//! Node added or removed renumbers nothing, because nothing is numbered
//! (display-config.md §3, DC-5).

use serde::Serialize;

use crate::state::models::{BeaconState, DeviceState, NodeState};

/// A wired probe's only quantity, and the field of an id that names none (the
/// pre-2026-10-30 `sticker:<dev_eui>` / `ble:<MAC>` shape a Hub may still echo).
const TEMPERATURE: &str = "temperature";

/// Length of a colon-separated MAC (`AA:BB:CC:DD:EE:FF`): a `ble:` id's field
/// starts after it, since the MAC itself contains colons.
const MAC_LEN: usize = 17;

/// How many rows the picker lets an operator check (display-config.md §5,
/// DC-3). Lower than the wire's own
/// [`fiber_protocol::display::MAX_DISPLAY_LINES`], deliberately: the LCD
/// shows four rows a page, and eight is two pages of paging.
pub const MAX_SELECTED: usize = 8;

/// One checkbox on the Unit tab.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SensorCatalogEntry {
    /// Opaque to the template and to the browser — echoed back verbatim by
    /// the form post and resolved by [`resolve`].
    pub id: String,
    /// `"ds18b20"` / `"sticker"` / `"ble"` — the template groups on it.
    pub source: &'static str,
    pub name: String,
    /// The quantity shown on the row (`temperature`, `humidity`, a Node
    /// registry field…): one Node or Beacon is one entry per quantity it
    /// reports (amendment 2026-10-30, #180).
    pub field: String,
    /// The field's English label, translated by the template's `field_label`.
    pub field_label: String,
    pub selected: bool,
}

/// Build the catalogue for one Hub, `selected` pre-computed from the
/// display lines the device last reported.
///
/// Probes first, then Nodes, then Beacons — the order the agent's built-in
/// layout renders, so a picked set reads the same way on screen.
pub fn catalog(device: &DeviceState) -> Vec<SensorCatalogEntry> {
    let selected = &device.config.display_lines;
    let mut entries = Vec::new();

    for sensor in &device.sensors {
        let id = format!("ds18b20:{}", sensor.line);
        entries.push(SensorCatalogEntry {
            name: probe_name(sensor.name.as_deref(), sensor.line),
            selected: is_selected(selected, &id),
            source: "ds18b20",
            field: TEMPERATURE.to_string(),
            field_label: "Temperature".to_string(),
            id,
        });
    }

    for (node_key, node) in &device.nodes {
        for (field, label) in node_fields(node) {
            let id = format!("sticker:{node_key}:{field}");
            entries.push(SensorCatalogEntry {
                name: node_name(node),
                selected: is_selected(selected, &id),
                source: "sticker",
                field,
                field_label: label,
                id,
            });
        }
    }

    for (mac, beacon) in &device.ble_sensors {
        // Registered tags only: a stranger the radio overheard is not a
        // display target, and would vanish from the list on the next scan
        // (`BeaconState::discovered`, where `None` reads as registered).
        if beacon.discovered == Some(true) {
            continue;
        }
        for (field, label) in beacon_fields(beacon) {
            let id = format!("ble:{mac}:{field}");
            entries.push(SensorCatalogEntry {
                name: beacon_name(beacon),
                selected: is_selected(selected, &id),
                source: "ble",
                field: field.to_string(),
                field_label: label.to_string(),
                id,
            });
        }
    }

    entries
}

/// The quantities a Node offers: the registry fields it has reported, in its
/// own order; a Node that has reported nothing yet offers its temperature.
fn node_fields(node: &NodeState) -> Vec<(String, String)> {
    let fields: Vec<(String, String)> = node
        .fields
        .keys()
        .filter_map(|name| {
            fiber_protocol::fields::lorawan::lorawan_field(name)
                .map(|f| (name.clone(), f.label.to_string()))
        })
        .collect();
    if fields.is_empty() {
        vec![(TEMPERATURE.to_string(), "Temperature".to_string())]
    } else {
        fields
    }
}

/// The quantities a Beacon offers: those of the agent's `BLE_FIELDS` it has
/// reported and its sensor mask leaves on. Only sensor channels: battery and
/// RSSI are link diagnostics, not offered. A tag that has reported nothing yet
/// offers its temperature.
fn beacon_fields(beacon: &BeaconState) -> Vec<(&'static str, &'static str)> {
    let reported = [
        ("temperature", "Temperature", beacon.temperature.is_some()),
        ("humidity", "Humidity", beacon.humidity.is_some()),
        (
            "movement",
            "Movement",
            beacon.movement_count.is_some() || beacon.moving.is_some(),
        ),
        ("pitch", "Pitch", beacon.pitch.is_some()),
        ("roll", "Roll", beacon.roll.is_some()),
    ];
    let fields: Vec<_> = reported
        .into_iter()
        .filter(|(f, _, on)| *on && sensor_enabled(beacon, f))
        .map(|(f, l, _)| (f, l))
        .collect();
    if fields.is_empty() {
        vec![(TEMPERATURE, "Temperature")]
    } else {
        fields
    }
}

/// Wired line label: its configured name, else "Line N" 1-based, which is
/// how the Sensors tab numbers the same eight lines (devices.hub.md §3.2).
fn probe_name(name: Option<&str>, line: u8) -> String {
    match name.map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => name.to_string(),
        None => crate::routes::tr("monitoring.shared.line_fallback")
            .replace("{n}", &(line + 1).to_string()),
    }
}

fn node_name(node: &NodeState) -> String {
    first_non_empty(&[
        node.name.as_deref(),
        node.serial_number.as_deref(),
        Some(&node.node_key),
    ])
}

fn beacon_name(beacon: &BeaconState) -> String {
    first_non_empty(&[beacon.name.as_deref(), Some(&beacon.mac)])
}

fn first_non_empty(candidates: &[Option<&str>]) -> String {
    candidates
        .iter()
        .flatten()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// Does this catalogue id address one of the lines the device reports?
/// Compares the resolved address, so a Hub that sends its own casing (a
/// lowercase MAC, an uppercase DevEUI) still pre-checks the right box.
fn is_selected(lines: &[fiber_protocol::DisplayLine], id: &str) -> bool {
    let Some(target) = resolve(id) else {
        return false;
    };
    lines.iter().any(|line| same_address(line, &target))
}

fn same_address(a: &fiber_protocol::DisplayLine, b: &fiber_protocol::DisplayLine) -> bool {
    use fiber_protocol::DisplayLineSource as S;
    if a.source != b.source {
        return false;
    }
    if a.field != b.field {
        return false;
    }
    match a.source {
        S::Ds18b20 => a.line == b.line,
        S::Node => match (a.dev_eui.as_deref(), b.dev_eui.as_deref()) {
            (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
            _ => false,
        },
        S::Ble => match (a.mac.as_deref(), b.mac.as_deref()) {
            (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
            _ => false,
        },
    }
}

/// Catalogue id -> the display line it stands for, with the default label
/// (none: the device derives it from the sensor's own name) and default
/// format. `None` for anything that is not a well-formed id — an unknown
/// probe index, a DevEUI that is not 16 hex, a mis-shaped MAC — so the
/// handler answers "unknown sensor" instead of relaying a line the device
/// would reject.
pub fn resolve(id: &str) -> Option<fiber_protocol::DisplayLine> {
    use fiber_protocol::{DisplayLine, DisplayLineFormat, DisplayLineSource};

    let (prefix, rest) = id.split_once(':')?;
    // `<address>[:<field>]`: a MAC carries colons of its own, so a `ble:` id
    // splits after the MAC's fixed length rather than at its first colon.
    let (address, field) = match prefix {
        "ble" if rest.len() > MAC_LEN => {
            let (mac, tail) = rest.split_at(MAC_LEN);
            (mac, tail.strip_prefix(':')?)
        }
        "ble" => (rest, TEMPERATURE),
        _ => rest.split_once(':').unwrap_or((rest, TEMPERATURE)),
    };
    let field = field.to_string();
    let rest = address;
    let line = match prefix {
        "ds18b20" => DisplayLine {
            source: DisplayLineSource::Ds18b20,
            line: Some(rest.parse::<u8>().ok()?),
            dev_eui: None,
            mac: None,
            field: field.clone(),
            label: None,
            format: DisplayLineFormat::default(),
        },
        "sticker" => DisplayLine {
            source: DisplayLineSource::Node,
            line: None,
            // The store keys Nodes lowercase; the agent lowercases on load.
            dev_eui: Some(rest.to_ascii_lowercase()),
            mac: None,
            field: field.clone(),
            label: None,
            format: DisplayLineFormat::default(),
        },
        "ble" => DisplayLine {
            source: DisplayLineSource::Ble,
            line: None,
            dev_eui: None,
            // Uppercase is the EYE subsystem's canonical MAC case.
            mac: Some(rest.to_ascii_uppercase()),
            field,
            label: None,
            format: DisplayLineFormat::default(),
        },
        _ => return None,
    };
    fiber_protocol::validate_display_line(&line).ok()?;
    Some(line)
}

/// Whether the Beacon's sensor mask (its read-back BLE config) leaves the
/// sensor behind `field` on. A tag whose config has not been read back yet is
/// not filtered.
fn sensor_enabled(beacon: &BeaconState, field: &str) -> bool {
    let sensor = match field {
        "temperature" => "temperature",
        "humidity" => "humidity",
        "movement" | "pitch" | "roll" => "movement",
        _ => return true,
    };
    let Some(config) = beacon.ble_config.as_ref() else {
        return true;
    };
    match (&config.sensors, &config.active_sensors) {
        (Some(map), _) => map.get(sensor).copied().unwrap_or(false),
        (None, Some(active)) => active.iter().any(|a| a == sensor),
        (None, None) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fiber_protocol::{DisplayLine, DisplayLineFormat, DisplayLineSource};

    const EUI: &str = "70b3d5e050011234";
    const MAC: &str = "AA:BB:CC:DD:EE:FF";

    fn device() -> DeviceState {
        let mut device = DeviceState::new("hub1".to_string());
        device.sensors[2].name = Some("Fridge A".to_string());
        device
            .nodes
            .insert(EUI.to_string(), NodeState::new(EUI.to_string(), 0));
        let mut beacon = BeaconState::new(MAC.to_string(), 0);
        beacon.name = Some("Tag 1".to_string());
        device.ble_sensors.insert(MAC.to_string(), beacon);
        device
    }

    #[test]
    fn catalog_lists_probes_then_nodes_then_beacons() {
        let entries = catalog(&device());
        assert_eq!(entries.len(), 8 + 1 + 1);
        assert_eq!(entries[0].id, "ds18b20:0");
        assert_eq!(entries[2].name, "Fridge A");
        assert_eq!(entries[8].id, format!("sticker:{EUI}:temperature"));
        assert_eq!(entries[9].id, format!("ble:{MAC}:temperature"));
        assert_eq!(entries[9].name, "Tag 1");
        assert!(entries.iter().all(|e| !e.selected));
    }

    #[test]
    fn a_nameless_probe_falls_back_to_its_1_based_line() {
        let entries = catalog(&device());
        assert!(entries[0].name.contains('1'), "{}", entries[0].name);
    }

    #[test]
    fn every_id_round_trips_to_a_valid_display_line() {
        for entry in catalog(&device()) {
            let line = resolve(&entry.id).unwrap_or_else(|| panic!("{} resolves", entry.id));
            assert!(
                fiber_protocol::validate_display_line(&line).is_ok(),
                "{line:?}"
            );
            match entry.source {
                "ds18b20" => assert!(line.line.is_some()),
                "sticker" => assert_eq!(line.dev_eui.as_deref(), Some(EUI)),
                "ble" => assert_eq!(line.mac.as_deref(), Some(MAC)),
                other => panic!("unexpected source {other}"),
            }
        }
    }

    #[test]
    fn a_malformed_or_unknown_id_resolves_to_nothing() {
        for id in [
            "",
            "ds18b20:9",
            "ds18b20:x",
            "sticker:zzz",
            "ble:AABB",
            "what:1",
        ] {
            assert!(resolve(id).is_none(), "{id}");
        }
    }

    /// The pre-check is an address match: the device's own casing differs
    /// from the store's, and a reordered list must not move a tick mark.
    #[test]
    fn selected_follows_the_address_not_the_position() {
        let mut device = device();
        device.config.display_lines = vec![
            DisplayLine {
                source: DisplayLineSource::Node,
                line: None,
                dev_eui: Some(EUI.to_uppercase()),
                mac: None,
                field: "temperature".to_string(),
                label: None,
                format: DisplayLineFormat::default(),
            },
            DisplayLine {
                source: DisplayLineSource::Ds18b20,
                line: Some(2),
                dev_eui: None,
                mac: None,
                field: "temperature".to_string(),
                label: None,
                format: DisplayLineFormat::default(),
            },
        ];
        let entries = catalog(&device);
        let picked: Vec<&str> = entries
            .iter()
            .filter(|e| e.selected)
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(
            picked,
            vec![
                "ds18b20:2".to_string(),
                format!("sticker:{EUI}:temperature")
            ]
        );
    }

    /// One entry per reported quantity, each resolving to its own field; the
    /// pre-check matches the field too.
    #[test]
    fn a_node_and_a_beacon_offer_each_quantity_they_report() {
        let mut device = device();
        let node = device.nodes.get_mut(EUI).unwrap();
        node.fields.insert("temperature".into(), 4.2);
        node.fields.insert("humidity".into(), 55.0);
        let beacon = device.ble_sensors.get_mut(MAC).unwrap();
        beacon.temperature = Some(5.0);
        beacon.humidity = Some(60);
        beacon.battery_mv = Some(3000);
        device.config.display_lines = vec![resolve(&format!("ble:{MAC}:humidity")).unwrap()];

        let entries = catalog(&device);
        let ids: Vec<&str> = entries.iter().skip(8).map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                format!("sticker:{EUI}:humidity").as_str(),
                format!("sticker:{EUI}:temperature").as_str(),
                format!("ble:{MAC}:temperature").as_str(),
                format!("ble:{MAC}:humidity").as_str(),
            ]
        );
        let picked: Vec<&str> = entries
            .iter()
            .filter(|e| e.selected)
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(picked, vec![format!("ble:{MAC}:humidity").as_str()]);
        for e in &entries {
            assert_eq!(resolve(&e.id).unwrap().field, e.field, "{}", e.id);
        }
    }

    #[test]
    fn an_id_without_a_field_still_means_temperature() {
        assert_eq!(resolve(&format!("ble:{MAC}")).unwrap().field, "temperature");
        assert_eq!(
            resolve(&format!("sticker:{EUI}")).unwrap().field,
            "temperature"
        );
        assert!(resolve(&format!("ble:{MAC}:colour")).is_none());
        assert!(resolve(&format!("sticker:{EUI}:colour")).is_none());
    }

    #[test]
    fn a_discovered_stranger_is_not_a_display_target() {
        let mut device = device();
        let mut stranger = BeaconState::new("11:22:33:44:55:66".to_string(), 0);
        stranger.discovered = Some(true);
        device.ble_sensors.insert(stranger.mac.clone(), stranger);
        let entries = catalog(&device);
        assert!(!entries.iter().any(|e| e.id.contains("11:22:33")));
    }

    /// A quantity whose sensor the Beacon's read-back mask turns off is not
    /// offered, and battery and RSSI never are.
    #[test]
    fn a_beacon_offers_only_what_its_sensor_mask_leaves_on() {
        let mut device = device();
        let beacon = device.ble_sensors.get_mut(MAC).unwrap();
        beacon.temperature = Some(5.0);
        beacon.humidity = Some(60);
        beacon.battery_mv = Some(3000);
        beacon.rssi = Some(-60);
        beacon.ble_config = Some(fiber_protocol::payloads::eye::EyeBleConfig {
            advertising_interval_ms: None,
            tx_power_dbm: None,
            protocol_type: None,
            protocol: None,
            sensors: None,
            active_sensors: Some(vec!["temperature".into()]),
            read_at: None,
            verified: Some(true),
            recording: None,
            logging_interval_min: None,
        });

        let fields: Vec<String> = catalog(&device)
            .into_iter()
            .filter(|e| e.source == "ble")
            .map(|e| e.field)
            .collect();
        assert_eq!(fields, vec!["temperature"]);
    }
}
