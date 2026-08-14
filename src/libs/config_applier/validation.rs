//! Configuration validation for safe updates

use crate::libs::config::{DisplayLine, DisplayLineSource};

/// Validate temperature threshold ordering
pub struct ConfigValidator;

impl ConfigValidator {
    /// Validate sensor threshold ordering (4-level system)
    ///
    /// Ensures: critical_low < warning_low < warning_high < critical_high
    pub fn validate_threshold_ordering(
        critical_low: f32,
        warning_low: f32,
        warning_high: f32,
        critical_high: f32,
    ) -> Result<(), String> {
        if critical_low >= warning_low {
            return Err(format!(
                "critical_low ({}) must be less than warning_low ({})",
                critical_low, warning_low
            ));
        }

        if warning_low >= warning_high {
            return Err(format!(
                "warning_low ({}) must be less than warning_high ({})",
                warning_low, warning_high
            ));
        }

        if warning_high >= critical_high {
            return Err(format!(
                "warning_high ({}) must be less than critical_high ({})",
                warning_high, critical_high
            ));
        }

        Ok(())
    }

    /// Validate temperature is in reasonable range for medical device
    pub fn validate_temperature_range(temp: f32, field_name: &str) -> Result<(), String> {
        // Medical devices typically operate in range -50°C to 100°C
        if !(-50.0..=100.0).contains(&temp) {
            return Err(format!(
                "{} ({}) is outside valid range (-50 to 100°C)",
                field_name, temp
            ));
        }

        Ok(())
    }

    /// Validate all thresholds for a sensor line
    pub fn validate_sensor_thresholds(
        line: u8,
        critical_low: f32,
        warning_low: f32,
        warning_high: f32,
        critical_high: f32,
    ) -> Result<(), String> {
        // Validate line number
        if line > 7 {
            return Err(format!("Invalid line number: {} (must be 0-7)", line));
        }

        // Validate individual temperatures
        Self::validate_temperature_range(critical_low, "critical_low")?;
        Self::validate_temperature_range(warning_low, "warning_low")?;
        Self::validate_temperature_range(warning_high, "warning_high")?;
        Self::validate_temperature_range(critical_high, "critical_high")?;

        // Validate ordering
        Self::validate_threshold_ordering(critical_low, warning_low, warning_high, critical_high)?;

        Ok(())
    }

    /// Validate sensor interval settings
    ///
    /// Ensures:
    /// - sample_interval_ms >= 100ms (minimum sampling rate)
    /// - report_interval_ms <= 24 hours
    /// - sample_interval_ms <= aggregation_interval_ms <= report_interval_ms
    pub fn validate_intervals(
        sample_interval_ms: u64,
        aggregation_interval_ms: u64,
        report_interval_ms: u64,
    ) -> Result<(), String> {
        // Minimum sample interval of 100ms
        if sample_interval_ms < 100 {
            return Err(format!(
                "sample_interval_ms ({}) must be >= 100ms",
                sample_interval_ms
            ));
        }

        // Maximum report interval of 24 hours
        const MAX_INTERVAL_MS: u64 = 24 * 60 * 60 * 1000; // 86,400,000ms
        if report_interval_ms > MAX_INTERVAL_MS {
            return Err(format!(
                "report_interval_ms ({}) must be <= 24 hours ({}ms)",
                report_interval_ms, MAX_INTERVAL_MS
            ));
        }

        // Logical ordering: sample <= aggregation <= report
        if sample_interval_ms > aggregation_interval_ms {
            return Err(format!(
                "sample_interval_ms ({}) must be <= aggregation_interval_ms ({})",
                sample_interval_ms, aggregation_interval_ms
            ));
        }

        if aggregation_interval_ms > report_interval_ms {
            return Err(format!(
                "aggregation_interval_ms ({}) must be <= report_interval_ms ({})",
                aggregation_interval_ms, report_interval_ms
            ));
        }

        Ok(())
    }
}

/// Maximum length of a user-set device label in bytes. Display + MQTT can
/// in principle carry more, but 64 was the historical limit on the MQTT
/// path and we keep that contract.
pub const MAX_DEVICE_LABEL_LEN: usize = 64;

/// Characters not allowed inside a device label:
/// - `/`, `+`, `#`: MQTT topic wildcards / separators. Embedding any of
///   these in `device_label` would either split the topic or get
///   interpreted as a wildcard subscription.
/// - Everything outside `0x20..=0x7E`: anything that's not printable
///   ASCII. The LCD renderer only ships an ASCII font and Unicode
///   characters render as boxes; control bytes / null / newline could
///   also corrupt log lines and downstream parsers.
///
/// Note: space (`0x20`) is allowed so users can write "Ward 3 Freezer".
pub fn validate_device_label(label: &str) -> Result<(), String> {
    validate_printable_ascii(label, "Device label", MAX_DEVICE_LABEL_LEN)?;

    // Additional rule specific to the device label: it is interpolated into
    // MQTT topics, so the wildcard/separator characters are out. Display line
    // labels don't need this — they never reach a topic.
    for (idx, b) in label.bytes().enumerate() {
        if matches!(b, b'/' | b'+' | b'#') {
            return Err(format!(
                "Device label contains MQTT-reserved character {:?} at byte {}",
                b as char, idx,
            ));
        }
    }
    Ok(())
}

/// Non-empty, at most `max` bytes, and printable ASCII only (`0x20..=0x7E`).
///
/// The LCD only ships an ASCII font, so Unicode renders as boxes, and control
/// bytes / null / newline would corrupt log lines and downstream parsers.
/// Space is allowed so users can write "Ward 3 Freezer".
///
/// `what` names the field in the error message, e.g. "Device label".
pub fn validate_printable_ascii(value: &str, what: &str, max: usize) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{} cannot be empty", what));
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
                "{} contains non-printable-ASCII byte 0x{:02X} at byte {}",
                what, b, idx,
            ));
        }
    }
    Ok(())
}

/// Maximum number of configured overview lines. Four rows per page, so this
/// is four pages — beyond that the user is paging through more screens than
/// they can reasonably track on a 128x64 panel.
pub const MAX_DISPLAY_LINES: usize = 16;

/// Maximum length of a custom row label: one full 128 px row at 6 px/char.
pub const MAX_DISPLAY_LABEL_LEN: usize = 21;

/// Maximum decimal places for a formatted value.
pub const MAX_DISPLAY_DECIMALS: u8 = 3;

/// Highest addressable DS18B20 line index.
const MAX_DS18B20_LINE: u8 = 7;

/// Pseudo-fields that are not in the LoRaWAN field registry because they
/// describe the link or the sensor as a whole rather than a measurement.
const NODE_PSEUDO_FIELDS: &[&str] = &["rssi", "snr", "status"];

/// Fields available for a DS18B20 probe. The 1-Wire path carries no battery,
/// RSSI or humidity — only a temperature and an alarm state.
const DS18B20_FIELDS: &[&str] = &["temperature", "status"];

/// Fields available for an EYE BLE tag.
///
/// Deliberately the same canonical names the rest of the EYE stack already uses
/// (`temperature`, `humidity`, `battery`, `movement` are the tag's threshold
/// field names) rather than the `BeaconTagState` struct-field names
/// (`temperature_c`, `humidity_pct`, `battery_mv`, `movement_count`). That way
/// `unit_for_field` / `default_decimals` already do the right thing for
/// `temperature`, `humidity`, `rssi` and `status` without a second table.
///
/// The tag's booleans (`magnet_detected`, `low_battery`) are deliberately absent:
/// a row renders either a number or the four-character alarm text, and a boolean
/// would need a third formatting mode.
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

/// A BLE MAC as the EYE subsystem writes it: six uppercase hex octets, colons.
/// Case is checked by the caller-facing normalisation (config load / command
/// validation uppercase first), so this only has to police the shape.
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

/// Validate one configured display line.
///
/// Rejects cross-source field/address mixes (e.g. a `ds18b20` line carrying a
/// `dev_eui`) rather than silently ignoring the stray key, so a Viewer bug
/// surfaces as a rejected command instead of a row that renders the wrong
/// sensor.
pub fn validate_display_line(line: &DisplayLine) -> Result<(), String> {
    match line.source {
        DisplayLineSource::Ds18b20 => {
            let idx = line
                .line
                .ok_or_else(|| "ds18b20 display line requires 'line'".to_string())?;
            if idx > MAX_DS18B20_LINE {
                return Err(format!(
                    "ds18b20 display line index must be 0-{} (got {})",
                    MAX_DS18B20_LINE, idx,
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
                    "node dev_eui must be 16 hex characters (got {:?})",
                    dev_eui,
                ));
            }
            if line.line.is_some() {
                return Err("node display line must not set 'line'".to_string());
            }
            if line.mac.is_some() {
                return Err("node display line must not set 'mac'".to_string());
            }
            let known = crate::libs::lorawan::registry::lookup(&line.field).is_some()
                || NODE_PSEUDO_FIELDS.contains(&line.field.as_str());
            if !known {
                return Err(format!(
                    "unknown node field {:?} (not in the LoRaWAN field registry, and not one of: {})",
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
                    "ble mac must be six colon-separated hex octets (got {:?})",
                    mac,
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
                "Display line decimals must be 0-{} (got {})",
                MAX_DISPLAY_DECIMALS, decimals,
            ));
        }
    }

    Ok(())
}

/// Validate a whole custom-line list. An empty list is valid and means
/// "restore the built-in layout".
pub fn validate_display_custom_lines(lines: &[DisplayLine]) -> Result<(), String> {
    if lines.len() > MAX_DISPLAY_LINES {
        return Err(format!(
            "At most {} display lines are supported (got {})",
            MAX_DISPLAY_LINES,
            lines.len(),
        ));
    }
    for (idx, line) in lines.iter().enumerate() {
        validate_display_line(line).map_err(|e| format!("display line {}: {}", idx, e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_threshold_ordering() {
        let result = ConfigValidator::validate_threshold_ordering(32.0, 35.0, 39.0, 42.0);
        assert!(result.is_ok());
    }

    #[test]
    fn test_invalid_threshold_ordering() {
        // critical_low >= warning_low
        let result = ConfigValidator::validate_threshold_ordering(36.0, 35.0, 39.0, 42.0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("critical_low"));
    }

    #[test]
    fn test_temperature_range_validation() {
        assert!(ConfigValidator::validate_temperature_range(36.5, "test").is_ok());
        assert!(ConfigValidator::validate_temperature_range(-50.0, "test").is_ok());
        assert!(ConfigValidator::validate_temperature_range(100.0, "test").is_ok());
        assert!(ConfigValidator::validate_temperature_range(-51.0, "test").is_err());
        assert!(ConfigValidator::validate_temperature_range(101.0, "test").is_err());
    }

    #[test]
    fn test_sensor_thresholds_validation() {
        // Valid thresholds
        assert!(ConfigValidator::validate_sensor_thresholds(0, 32.0, 35.0, 39.0, 42.0).is_ok());

        // Invalid line number
        assert!(ConfigValidator::validate_sensor_thresholds(99, 32.0, 35.0, 39.0, 42.0).is_err());

        // Temperature out of range
        assert!(ConfigValidator::validate_sensor_thresholds(0, 32.0, 35.0, 39.0, 150.0).is_err());

        // Invalid ordering
        assert!(ConfigValidator::validate_sensor_thresholds(0, 32.0, 38.0, 36.0, 42.0).is_err());
    }

    // ---- device label validation ------------------------------------------------

    #[test]
    fn device_label_accepts_simple_ascii() {
        assert!(validate_device_label("FIBER-001").is_ok());
        assert!(validate_device_label("Ward 3 Freezer").is_ok());
        assert!(validate_device_label("a").is_ok(), "single char ok");
    }

    #[test]
    fn device_label_rejects_empty() {
        let err = validate_device_label("").unwrap_err();
        assert!(err.to_lowercase().contains("empty"), "got: {err}");
    }

    #[test]
    fn device_label_rejects_too_long() {
        let s = "A".repeat(MAX_DEVICE_LABEL_LEN + 1);
        let err = validate_device_label(&s).unwrap_err();
        assert!(
            err.contains(&MAX_DEVICE_LABEL_LEN.to_string()),
            "got: {err}"
        );
    }

    #[test]
    fn device_label_accepts_exact_max_length() {
        let s = "A".repeat(MAX_DEVICE_LABEL_LEN);
        assert!(validate_device_label(&s).is_ok());
    }

    #[test]
    fn device_label_rejects_mqtt_breakers() {
        for bad in ["a/b", "a+b", "a#b", "/leading", "trailing#", "with + sign"] {
            let err = validate_device_label(bad).unwrap_err();
            assert!(
                err.to_lowercase().contains("mqtt"),
                "expected MQTT mention for {:?}, got: {}",
                bad,
                err,
            );
        }
    }

    #[test]
    fn device_label_rejects_unicode() {
        // Includes accented (Portuguese), emoji, and CJK to be thorough.
        for bad in ["Câmara", "Ward 🥶", "病房", "naïve"] {
            assert!(
                validate_device_label(bad).is_err(),
                "should reject non-ASCII: {:?}",
                bad,
            );
        }
    }

    #[test]
    fn device_label_rejects_control_and_null() {
        for bad in ["a\0b", "a\nb", "a\tb", "\x7Fdel"] {
            assert!(
                validate_device_label(bad).is_err(),
                "should reject control byte in {:?}",
                bad,
            );
        }
    }

    #[test]
    fn device_label_accepts_all_other_punctuation() {
        // Verify common safe symbols round-trip — these are well within
        // printable ASCII and not MQTT-reserved.
        for ok in ["Lab-A_3", "v1.0", "@home", "(spare)", "PASS!", "100%"] {
            assert!(validate_device_label(ok).is_ok(), "should accept {:?}", ok);
        }
    }
}

#[cfg(test)]
mod display_line_tests {
    use super::*;
    use crate::libs::config::DisplayLineFormat;

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

    fn node(dev_eui: &str, field: &str) -> DisplayLine {
        DisplayLine {
            source: DisplayLineSource::Node,
            line: None,
            dev_eui: Some(dev_eui.to_string()),
            mac: None,
            field: field.to_string(),
            label: None,
            format: DisplayLineFormat::default(),
        }
    }

    fn ble(mac: &str, field: &str) -> DisplayLine {
        DisplayLine {
            source: DisplayLineSource::Ble,
            line: None,
            dev_eui: None,
            mac: Some(mac.to_string()),
            field: field.to_string(),
            label: None,
            format: DisplayLineFormat::default(),
        }
    }

    const EUI: &str = "70b3d57ed0051f2a";
    const MAC: &str = "7C:D9:F4:13:10:DE";

    #[test]
    fn accepts_valid_ds18b20_and_node_lines() {
        assert!(validate_display_line(&ds(Some(0), "temperature")).is_ok());
        assert!(validate_display_line(&ds(Some(7), "status")).is_ok());
        assert!(validate_display_line(&node(EUI, "humidity")).is_ok());
    }

    #[test]
    fn rejects_ds18b20_line_over_7() {
        let err = validate_display_line(&ds(Some(8), "temperature")).unwrap_err();
        assert!(err.contains("0-7"), "got: {}", err);
    }

    #[test]
    fn rejects_ds18b20_without_line_index() {
        assert!(validate_display_line(&ds(None, "temperature")).is_err());
    }

    #[test]
    fn rejects_ds18b20_with_dev_eui() {
        let mut line = ds(Some(0), "temperature");
        line.dev_eui = Some(EUI.to_string());
        let err = validate_display_line(&line).unwrap_err();
        assert!(err.contains("dev_eui"), "got: {}", err);
    }

    #[test]
    fn rejects_node_with_line_index() {
        let mut line = node(EUI, "temperature");
        line.line = Some(2);
        let err = validate_display_line(&line).unwrap_err();
        assert!(err.contains("'line'"), "got: {}", err);
    }

    #[test]
    fn rejects_unknown_ds18b20_field() {
        // Valid for a node, but the 1-Wire path has no humidity.
        let err = validate_display_line(&ds(Some(0), "humidity")).unwrap_err();
        assert!(err.contains("unknown ds18b20 field"), "got: {}", err);
    }

    #[test]
    fn accepts_every_registry_field_and_pseudo_field_for_nodes() {
        for def in crate::libs::lorawan::registry::REGISTRY {
            assert!(
                validate_display_line(&node(EUI, def.name)).is_ok(),
                "registry field {} should be selectable",
                def.name,
            );
        }
        for name in NODE_PSEUDO_FIELDS {
            assert!(
                validate_display_line(&node(EUI, name)).is_ok(),
                "pseudo-field {} should be selectable",
                name,
            );
        }
    }

    #[test]
    fn rejects_unknown_node_field() {
        let err = validate_display_line(&node(EUI, "battery_percent")).unwrap_err();
        assert!(err.contains("unknown node field"), "got: {}", err);
    }

    #[test]
    fn rejects_bad_dev_eui() {
        for bad in [
            "70b3d57ed0051f2",
            "70b3d57ed0051f2ab",
            "70b3d57ed0051fZZ",
            "",
        ] {
            let err = validate_display_line(&node(bad, "temperature")).unwrap_err();
            assert!(err.contains("16 hex"), "for {:?} got: {}", bad, err);
        }
    }

    #[test]
    fn rejects_node_without_dev_eui() {
        let mut line = node(EUI, "temperature");
        line.dev_eui = None;
        assert!(validate_display_line(&line).is_err());
    }

    // ---- ble source ------------------------------------------------------

    #[test]
    fn accepts_every_ble_field() {
        for name in BLE_FIELDS {
            assert!(
                validate_display_line(&ble(MAC, name)).is_ok(),
                "ble field {} should be selectable",
                name,
            );
        }
    }

    #[test]
    fn rejects_unknown_ble_field() {
        // Real BeaconTagState members, but not ones a row can render: the first is
        // a boolean, the second is the struct-field name rather than the
        // canonical one the catalog offers.
        for bad in ["magnet_detected", "temperature_c", "snr"] {
            let err = validate_display_line(&ble(MAC, bad)).unwrap_err();
            assert!(
                err.contains("unknown ble field"),
                "for {:?} got: {}",
                bad,
                err
            );
        }
    }

    #[test]
    fn rejects_ble_without_mac() {
        let mut line = ble(MAC, "temperature");
        line.mac = None;
        let err = validate_display_line(&line).unwrap_err();
        assert!(err.contains("requires 'mac'"), "got: {}", err);
    }

    #[test]
    fn rejects_bad_ble_mac() {
        for bad in [
            "7C:D9:F4:13:10",       // five octets
            "7C:D9:F4:13:10:DE:AB", // seven
            "7C:D9:F4:13:10:ZZ",    // not hex
            "7CD9F41310DE",         // no separators
            "",
        ] {
            let err = validate_display_line(&ble(bad, "temperature")).unwrap_err();
            assert!(err.contains("hex octets"), "for {:?} got: {}", bad, err);
        }
    }

    /// A lowercase MAC is *shape*-valid here on purpose: config load and the
    /// command validator both uppercase before this runs, so rejecting case
    /// would only fire on a path that cannot occur.
    #[test]
    fn accepts_a_lowercase_mac_shape() {
        assert!(validate_display_line(&ble("7c:d9:f4:13:10:de", "temperature")).is_ok());
    }

    #[test]
    fn rejects_cross_source_address_mixes() {
        let mut ble_with_eui = ble(MAC, "temperature");
        ble_with_eui.dev_eui = Some(EUI.to_string());
        assert!(validate_display_line(&ble_with_eui)
            .unwrap_err()
            .contains("must not set 'dev_eui'"));

        let mut ble_with_line = ble(MAC, "temperature");
        ble_with_line.line = Some(0);
        assert!(validate_display_line(&ble_with_line)
            .unwrap_err()
            .contains("must not set 'line'"));

        let mut node_with_mac = node(EUI, "temperature");
        node_with_mac.mac = Some(MAC.to_string());
        assert!(validate_display_line(&node_with_mac)
            .unwrap_err()
            .contains("must not set 'mac'"));

        let mut ds_with_mac = ds(Some(0), "temperature");
        ds_with_mac.mac = Some(MAC.to_string());
        assert!(validate_display_line(&ds_with_mac)
            .unwrap_err()
            .contains("must not set 'mac'"));
    }

    #[test]
    fn rejects_non_ascii_label() {
        let mut line = ds(Some(0), "temperature");
        line.label = Some("Kühlraum".to_string());
        let err = validate_display_line(&line).unwrap_err();
        assert!(err.contains("non-printable-ASCII"), "got: {}", err);
    }

    #[test]
    fn rejects_empty_and_overlong_label() {
        let mut line = ds(Some(0), "temperature");
        line.label = Some(String::new());
        assert!(validate_display_line(&line).is_err());

        line.label = Some("x".repeat(MAX_DISPLAY_LABEL_LEN + 1));
        let err = validate_display_line(&line).unwrap_err();
        assert!(err.contains("at most 21"), "got: {}", err);
    }

    #[test]
    fn rejects_decimals_over_3() {
        let mut line = ds(Some(0), "temperature");
        line.format.decimals = Some(4);
        let err = validate_display_line(&line).unwrap_err();
        assert!(err.contains("0-3"), "got: {}", err);

        line.format.decimals = Some(3);
        assert!(validate_display_line(&line).is_ok());
    }

    #[test]
    fn empty_list_is_valid_and_means_default_layout() {
        assert!(validate_display_custom_lines(&[]).is_ok());
    }

    #[test]
    fn rejects_more_than_16_lines() {
        let lines: Vec<DisplayLine> = (0..17).map(|_| ds(Some(0), "temperature")).collect();
        let err = validate_display_custom_lines(&lines).unwrap_err();
        assert!(err.contains("At most 16"), "got: {}", err);

        let lines: Vec<DisplayLine> = (0..16).map(|_| ds(Some(0), "temperature")).collect();
        assert!(validate_display_custom_lines(&lines).is_ok());
    }

    #[test]
    fn list_error_names_the_offending_index() {
        let lines = vec![ds(Some(0), "temperature"), ds(Some(9), "temperature")];
        let err = validate_display_custom_lines(&lines).unwrap_err();
        assert!(err.starts_with("display line 1:"), "got: {}", err);
    }

    #[test]
    fn duplicate_lines_on_the_same_source_are_allowed() {
        // Two rows on one node (e.g. temperature + battery) is the headline
        // use case from the feature request, not an error.
        let lines = vec![node(EUI, "ext_temperature_1"), node(EUI, "voltage")];
        assert!(validate_display_custom_lines(&lines).is_ok());
    }
}
