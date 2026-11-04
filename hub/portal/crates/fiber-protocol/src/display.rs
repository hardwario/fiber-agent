//! Physical-display line configuration — the `set_display_lines` payload
//! and its validator (plan/portal/display-config.md §3, WP-B/B1).
//!
//! The portal has to build `DisplayLine` JSON to send `set_display_lines`,
//! so the type belongs in the wire crate next to every other payload
//! rather than in a handler. The agent keeps its own copy in
//! `control-core/src/libs/config.rs` (`DisplayLine`) and validates with
//! `config_applier::validation::validate_display_custom_lines`; the two
//! are pinned together by [`tests::round_trips_the_agents_json`], which
//! spells out the exact JSON the agent's serde accepts and emits. Merging
//! the two copies means making `control-core` depend on this crate, which
//! it does not today — that is the agent owner's call, not the portal's.
//!
//! One knowing difference from the agent's validator: the node-field check
//! runs against [`crate::fields::lorawan`], which carries 8 `event`-kind
//! fields the agent's own registry never defines (see that module's
//! "Cross-check findings"). This side is therefore *looser* for those 8 —
//! the device still refuses them, so the failure surfaces as a rejected
//! command, never as a wrong row.

use serde::{Deserialize, Serialize};

/// Maximum number of configured overview lines the agent accepts
/// (`validation::MAX_DISPLAY_LINES`). The portal's picker caps itself
/// lower (8, display-config.md §5) — that cap is the picker's, this one is
/// the wire's.
pub const MAX_DISPLAY_LINES: usize = 16;

/// Maximum length of a custom row label: one full 128 px row at 6 px/char.
pub const MAX_DISPLAY_LABEL_LEN: usize = 21;

/// Maximum decimal places for a formatted value.
pub const MAX_DISPLAY_DECIMALS: u8 = 3;

/// Highest addressable DS18B20 line index.
const MAX_DS18B20_LINE: u8 = 7;

/// Pseudo-fields that are not in the Node field registry because they
/// describe the link or the sensor as a whole rather than a measurement.
pub const NODE_PSEUDO_FIELDS: &[&str] = &["rssi", "snr", "status"];

/// Fields available for a DS18B20 probe: the 1-Wire path carries no
/// battery, RSSI or humidity, only a temperature and an alarm state.
pub const DS18B20_FIELDS: &[&str] = &["temperature", "status"];

/// Fields available for an EYE BLE tag, verbatim from the agent's
/// `validation::BLE_FIELDS`.
pub const BLE_FIELDS: &[&str] = &[
    "temperature",
    "humidity",
    "battery",
    "rssi",
    "status",
    "movement",
    "pitch",
    "roll",
];

/// Where a configured display line reads its value from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayLineSource {
    /// A DS18B20 1-Wire probe on the Hub, addressed by its line index.
    Ds18b20,
    /// A ProXimos Node (STICKER), addressed by DevEUI. The wire spelling
    /// is `sticker`, which is the name on the drát.
    #[serde(rename = "sticker")]
    Node,
    /// An EYE BLE tag (Beacon), addressed by MAC.
    Ble,
}

/// One configured row of the Hub's sensor overview screen.
///
/// Sources are addressed concretely (`line`/`dev_eui`/`mac`), never by
/// ordinal: node ordering is a live sort, so an ordinal would silently
/// re-point at a different sensor when a sibling is added or removed
/// (display-config.md §3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayLine {
    pub source: DisplayLineSource,

    /// DS18B20 line index 0..=7. Required iff `source` is `ds18b20`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u8>,

    /// Node DevEUI, 16 hex characters. Required iff `source` is `sticker`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev_eui: Option<String>,

    /// Beacon MAC, `AA:BB:CC:DD:EE:FF` uppercase (the EYE subsystem's
    /// canonical case). Required iff `source` is `ble`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,

    /// Field selector — see [`DS18B20_FIELDS`], the Node registry plus
    /// [`NODE_PSEUDO_FIELDS`], and [`BLE_FIELDS`].
    pub field: String,

    /// Row label. Absent means "derive it from the sensor's own name",
    /// which is what the portal's picker sends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    #[serde(default, skip_serializing_if = "DisplayLineFormat::is_default")]
    pub format: DisplayLineFormat,
}

/// Per-line value formatting. Omitted from the wire entirely when every
/// option is at its default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayLineFormat {
    /// Append the field's unit (`°C`, `%`, `V`, …).
    #[serde(default = "default_true")]
    pub units: bool,

    /// Decimal places, 0..=3. `None` uses the per-field default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,

    /// Draw the alarm status character at the right edge of the row.
    #[serde(default = "default_true")]
    pub status_char: bool,
}

fn default_true() -> bool {
    true
}

impl Default for DisplayLineFormat {
    fn default() -> Self {
        Self {
            units: true,
            decimals: None,
            status_char: true,
        }
    }
}

impl DisplayLineFormat {
    /// True when every option is at its default, so `format` can be left
    /// off the wire instead of written out in full.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

fn is_mac_shaped(mac: &str) -> bool {
    let mut octets = 0;
    for part in mac.split(':') {
        if part.len() != 2 || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
        octets += 1;
    }
    octets == 6
}

/// Non-empty, at most `max` bytes, printable ASCII only — the agent's
/// `validation::validate_printable_ascii`, which the LCD's 6x8 font needs.
fn validate_printable_ascii(value: &str, what: &str, max: usize) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{what} cannot be empty"));
    }
    if value.len() > max {
        return Err(format!(
            "{} must be at most {} characters (got {})",
            what,
            max,
            value.len(),
        ));
    }
    for (idx, b) in value.bytes().enumerate() {
        if !(0x20..=0x7E).contains(&b) {
            return Err(format!(
                "{what} contains non-printable-ASCII byte 0x{b:02X} at byte {idx}",
            ));
        }
    }
    Ok(())
}

/// Validate one configured display line.
///
/// Rejects cross-source field/address mixes (a `ds18b20` line carrying a
/// `dev_eui`, say) rather than ignoring the stray key, so a portal bug
/// surfaces as a rejected command instead of a row rendering the wrong
/// sensor.
pub fn validate_display_line(line: &DisplayLine) -> Result<(), String> {
    match line.source {
        DisplayLineSource::Ds18b20 => {
            let idx = line
                .line
                .ok_or_else(|| "ds18b20 display line requires 'line'".to_string())?;
            if idx > MAX_DS18B20_LINE {
                return Err(format!(
                    "ds18b20 display line index must be 0-{MAX_DS18B20_LINE} (got {idx})",
                ));
            }
            if line.dev_eui.is_some() {
                return Err("ds18b20 display line must not set 'dev_eui'".to_string());
            }
            if line.mac.is_some() {
                return Err("ds18b20 display line must not set 'mac'".to_string());
            }
            if !DS18B20_FIELDS.contains(&line.field.as_str()) {
                return Err(format!(
                    "unknown ds18b20 field {:?} (available: {})",
                    line.field,
                    DS18B20_FIELDS.join(", "),
                ));
            }
        }
        DisplayLineSource::Node => {
            let dev_eui = line
                .dev_eui
                .as_deref()
                .ok_or_else(|| "node display line requires 'dev_eui'".to_string())?;
            if dev_eui.len() != 16 || !dev_eui.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(format!(
                    "node dev_eui must be 16 hex characters (got {dev_eui:?})",
                ));
            }
            if line.line.is_some() {
                return Err("node display line must not set 'line'".to_string());
            }
            if line.mac.is_some() {
                return Err("node display line must not set 'mac'".to_string());
            }
            let known = crate::fields::lorawan::lorawan_field(&line.field).is_some()
                || NODE_PSEUDO_FIELDS.contains(&line.field.as_str());
            if !known {
                return Err(format!(
                    "unknown node field {:?} (not in the Node field registry, and not one of: {})",
                    line.field,
                    NODE_PSEUDO_FIELDS.join(", "),
                ));
            }
        }
        DisplayLineSource::Ble => {
            let mac = line
                .mac
                .as_deref()
                .ok_or_else(|| "ble display line requires 'mac'".to_string())?;
            if !is_mac_shaped(mac) {
                return Err(format!(
                    "ble mac must be six colon-separated hex octets (got {mac:?})",
                ));
            }
            if line.line.is_some() {
                return Err("ble display line must not set 'line'".to_string());
            }
            if line.dev_eui.is_some() {
                return Err("ble display line must not set 'dev_eui'".to_string());
            }
            if !BLE_FIELDS.contains(&line.field.as_str()) {
                return Err(format!(
                    "unknown ble field {:?} (available: {})",
                    line.field,
                    BLE_FIELDS.join(", "),
                ));
            }
        }
    }

    if let Some(label) = line.label.as_deref() {
        validate_printable_ascii(label, "Display line label", MAX_DISPLAY_LABEL_LEN)?;
    }

    if let Some(decimals) = line.format.decimals {
        if decimals > MAX_DISPLAY_DECIMALS {
            return Err(format!(
                "Display line decimals must be 0-{MAX_DISPLAY_DECIMALS} (got {decimals})",
            ));
        }
    }

    Ok(())
}

/// Validate a whole `set_display_lines` list. An empty list is valid and
/// means "restore the built-in layout".
pub fn validate_display_custom_lines(lines: &[DisplayLine]) -> Result<(), String> {
    if lines.len() > MAX_DISPLAY_LINES {
        return Err(format!(
            "At most {} display lines are supported (got {})",
            MAX_DISPLAY_LINES,
            lines.len(),
        ));
    }
    for (idx, line) in lines.iter().enumerate() {
        validate_display_line(line).map_err(|e| format!("display line {idx}: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EUI: &str = "70b3d5e050011234";

    fn ds(line: Option<u8>, field: &str) -> DisplayLine {
        DisplayLine {
            source: DisplayLineSource::Ds18b20,
            line,
            dev_eui: None,
            mac: None,
            field: field.to_string(),
            label: None,
            format: DisplayLineFormat::default(),
        }
    }

    /// The drift pin against `control-core`'s own `DisplayLine`: this is
    /// the JSON its serde emits and accepts, key for key. A change on
    /// either side that this test does not mirror is the drift.
    #[test]
    fn round_trips_the_agents_json() {
        let json = r#"[
            {"source":"ds18b20","line":3,"field":"temperature"},
            {"source":"sticker","dev_eui":"70b3d5e050011234","field":"temperature","label":"Ward 3"},
            {"source":"ble","mac":"AA:BB:CC:DD:EE:FF","field":"humidity",
             "format":{"units":false,"decimals":2,"status_char":false}}
        ]"#;
        let lines: Vec<DisplayLine> = serde_json::from_str(json).expect("parses");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].line, Some(3));
        assert_eq!(lines[1].source, DisplayLineSource::Node);
        assert_eq!(lines[1].label.as_deref(), Some("Ward 3"));
        assert_eq!(lines[2].format.decimals, Some(2));
        assert!(validate_display_custom_lines(&lines).is_ok());

        // Serializing back drops every absent address and a default
        // `format`, which is what keeps the retained document small.
        let out = serde_json::to_value(&lines).expect("serializes");
        assert_eq!(
            out[0],
            serde_json::json!({"source": "ds18b20", "line": 3, "field": "temperature"}),
        );
        assert_eq!(out[1]["source"], "sticker");
        assert!(out[1].get("format").is_none(), "default format is omitted");
        assert_eq!(out[2]["format"]["decimals"], 2);

        // And a second pass is identical, so nothing is lost in either
        // direction.
        let again: Vec<DisplayLine> = serde_json::from_value(out).expect("re-parses");
        assert_eq!(again, lines);
    }

    #[test]
    fn empty_list_is_the_built_in_layout() {
        assert!(validate_display_custom_lines(&[]).is_ok());
    }

    #[test]
    fn rejects_a_probe_line_out_of_range_and_a_missing_address() {
        assert!(validate_display_line(&ds(Some(8), "temperature")).is_err());
        assert!(validate_display_line(&ds(None, "temperature")).is_err());
        assert!(validate_display_line(&ds(Some(0), "humidity")).is_err());
    }

    #[test]
    fn rejects_cross_source_addressing() {
        let mut line = ds(Some(0), "temperature");
        line.dev_eui = Some(EUI.to_string());
        assert!(validate_display_line(&line).is_err());
    }

    #[test]
    fn node_and_beacon_addresses_are_shape_checked() {
        let node = DisplayLine {
            source: DisplayLineSource::Node,
            line: None,
            dev_eui: Some(EUI.to_string()),
            mac: None,
            field: "temperature".to_string(),
            label: None,
            format: DisplayLineFormat::default(),
        };
        assert!(validate_display_line(&node).is_ok());

        let mut short = node.clone();
        short.dev_eui = Some("abcd".to_string());
        assert!(validate_display_line(&short).is_err());

        let beacon = DisplayLine {
            source: DisplayLineSource::Ble,
            line: None,
            dev_eui: None,
            mac: Some("AA:BB:CC:DD:EE:FF".to_string()),
            field: "temperature".to_string(),
            label: None,
            format: DisplayLineFormat::default(),
        };
        assert!(validate_display_line(&beacon).is_ok());

        let mut bad_mac = beacon.clone();
        bad_mac.mac = Some("AABBCCDDEEFF".to_string());
        assert!(validate_display_line(&bad_mac).is_err());
    }

    /// The v2.0.0 1-Wire Hub fields (#119) are selectable, and so are the pre-v2.0.0
    /// names a stored layout may still carry.
    #[test]
    fn node_lines_take_the_v200_and_the_older_field_names() {
        for field in [
            "w1_1_temperature",
            "w1_2_temperature_aux",
            "w1_3_tilt",
            "w1_4_accel_z",
            "temperature_baro",
            "ext_temperature_1",
            "machine_probe_humidity_2",
        ] {
            let line = DisplayLine {
                source: DisplayLineSource::Node,
                dev_eui: Some("70b3d5e050011234".to_string()),
                field: field.to_string(),
                ..ds(None, "temperature")
            };
            assert!(validate_display_line(&line).is_ok(), "{field}");
        }
    }

    #[test]
    fn rejects_more_than_the_wire_maximum() {
        let lines: Vec<DisplayLine> = (0..MAX_DISPLAY_LINES + 1)
            .map(|_| ds(Some(0), "temperature"))
            .collect();
        let err = validate_display_custom_lines(&lines).unwrap_err();
        assert!(err.contains("At most 16"), "got: {err}");
    }

    #[test]
    fn a_list_error_names_the_offending_index() {
        let lines = vec![ds(Some(0), "temperature"), ds(Some(9), "temperature")];
        let err = validate_display_custom_lines(&lines).unwrap_err();
        assert!(err.starts_with("display line 1:"), "got: {err}");
    }
}
