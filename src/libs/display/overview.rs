//! Pure layout and formatting logic for the configurable overview screen.
//!
//! Everything here is free of `St7920`, `embedded-graphics` and GPIO so it can
//! be unit-tested on the host — [`crate::drivers::display::St7920`] needs real
//! hardware to construct, so any logic left inside a `render_*` function is
//! effectively untestable. `screens.rs` keeps only the drawing calls.

use super::screens::truncate_chars;
use crate::libs::alarms::AlarmState;
use crate::libs::config::{DisplayLine, DisplayLineFormat, DisplayLineSource};
use crate::libs::lorawan::registry::{self, FieldKind};
use crate::libs::lorawan::state::{LoRaWANAlarmState, LoRaWANSensorState};
use crate::libs::sensors::state::SensorReading;

/// Characters that fit on one row: 128 px panel, 6 px glyph advance, minus the
/// 2 px left margin the rows are drawn at.
pub const ROW_CHARS: usize = 21;

/// Value shown when a field name isn't one this device can read. Only
/// reachable via a hand-edited config file — the MQTT command path rejects
/// unknown fields up front — so it reads as "misconfigured", distinct from the
/// `--.-` that means "configured fine, no data yet".
const UNKNOWN_FIELD_VALUE: &str = "?";

/// One fully-resolved overview row, ready to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedLine {
    /// Row label, already truncated to its character budget.
    pub label: String,
    /// Formatted value, e.g. `23.4°C`, `--.-°C`, `3.02V`, `CRIT`.
    pub value: String,
    /// Alarm status character, or `None` when the line disables it.
    pub status_char: Option<char>,
    /// True for a critical alarm — the row is drawn inverted.
    pub is_alarm: bool,
}

/// Unit suffix for a field, or `""` for fields that have none (counters and
/// the `status` pseudo-field).
pub fn unit_for_field(field: &str) -> &'static str {
    match field {
        "voltage" => "V",
        "pressure" => "hPa",
        "altitude" => "m",
        "illuminance" => "lx",
        "rssi" => "dBm",
        "snr" => "dB",
        "status" => "",
        // Covers temperature, ext_temperature_N and machine_probe_temperature_N.
        f if f.contains("temperature") => "°C",
        // Covers humidity and machine_probe_humidity_N.
        f if f.contains("humidity") => "%",
        _ => "",
    }
}

/// Decimal places to use when the line doesn't specify any.
pub fn default_decimals(field: &str) -> u8 {
    match field {
        "voltage" => 2,
        "snr" => 1,
        "humidity" | "pressure" | "illuminance" | "altitude" | "rssi" => 0,
        f if f.contains("humidity") => 0,
        f if matches!(registry::lookup(f).map(|d| d.kind), Some(FieldKind::Counter)) => 0,
        // Temperatures and anything else continuous.
        _ => 1,
    }
}

/// Decimal places actually used for a line.
fn effective_decimals(field: &str, format: &DisplayLineFormat) -> usize {
    format.decimals.unwrap_or_else(|| default_decimals(field)) as usize
}

/// The "no reading" placeholder, matching the width the real value would have
/// so a row doesn't visibly reflow when data arrives: `--.-°C`, `--%`, `--.--V`.
pub fn placeholder_for(field: &str, format: &DisplayLineFormat) -> String {
    if field == "status" {
        return "----".to_string();
    }
    let decimals = effective_decimals(field, format);
    let mut out = String::from("--");
    if decimals > 0 {
        out.push('.');
        for _ in 0..decimals {
            out.push('-');
        }
    }
    if format.units {
        out.push_str(unit_for_field(field));
    }
    out
}

/// Format a numeric field value, or the placeholder when it's unavailable.
pub fn format_field_value(field: &str, value: Option<f64>, format: &DisplayLineFormat) -> String {
    match value {
        None => placeholder_for(field, format),
        Some(v) => {
            let mut out = format!("{:.*}", effective_decimals(field, format), v);
            if format.units {
                out.push_str(unit_for_field(field));
            }
            out
        }
    }
}

/// Status character for a DS18B20 alarm state. Mirrors the built-in layout's
/// mapping so both paths read the same on the panel.
pub fn status_char_ds(state: AlarmState) -> char {
    match state {
        AlarmState::NeverConnected => '-',
        AlarmState::Disconnected => 'E',
        AlarmState::Reconnecting => 'W',
        AlarmState::Normal => 'N',
        AlarmState::Warning => 'W',
        AlarmState::Critical => 'C',
    }
}

/// Status character for a LoRaWAN alarm state.
///
/// Takes a reference because `LoRaWANAlarmState` is `Clone` but not `Copy`,
/// unlike its DS18B20 counterpart.
pub fn status_char_lora(state: &LoRaWANAlarmState) -> char {
    match state {
        LoRaWANAlarmState::Normal => 'N',
        LoRaWANAlarmState::Warning => 'W',
        LoRaWANAlarmState::Critical => 'C',
        LoRaWANAlarmState::Disconnected => 'E',
    }
}

/// Four-character alarm state text, for a line whose field is `status`.
pub fn alarm_text_ds(state: AlarmState) -> &'static str {
    match state {
        AlarmState::NeverConnected => "----",
        AlarmState::Disconnected => "DISC",
        AlarmState::Reconnecting => "RECN",
        AlarmState::Normal => "NORM",
        AlarmState::Warning => "WARN",
        AlarmState::Critical => "CRIT",
    }
}

/// Four-character alarm state text for a LoRaWAN sensor.
pub fn alarm_text_lora(state: &LoRaWANAlarmState) -> &'static str {
    match state {
        LoRaWANAlarmState::Normal => "NORM",
        LoRaWANAlarmState::Warning => "WARN",
        LoRaWANAlarmState::Critical => "CRIT",
        LoRaWANAlarmState::Disconnected => "DISC",
    }
}

/// Label to use when the line doesn't set one: the source sensor's configured
/// name, falling back to something that still identifies the row when the
/// sensor is unknown (an unprovisioned DevEUI, or an out-of-range probe index).
pub fn default_label(
    line: &DisplayLine,
    ds_names: &[String; 8],
    lorawan: &[LoRaWANSensorState],
) -> String {
    match line.source {
        DisplayLineSource::Ds18b20 => match line.line.map(usize::from) {
            Some(idx) if idx < ds_names.len() => ds_names[idx].clone(),
            _ => UNKNOWN_FIELD_VALUE.to_string(),
        },
        DisplayLineSource::Sticker => {
            let dev_eui = line.dev_eui.as_deref().unwrap_or_default();
            match find_sticker(lorawan, dev_eui) {
                Some(sensor) => sensor.name.clone(),
                // Last 4 hex digits, so an unprovisioned row is still
                // traceable back to the sticker the user meant.
                None => {
                    let count = dev_eui.chars().count();
                    let tail: String = dev_eui.chars().skip(count.saturating_sub(4)).collect();
                    format!("?{}", tail)
                }
            }
        }
    }
}

/// Fit a label and value onto one row, truncating the label as needed.
///
/// The value is never truncated: a clipped number is worse than a clipped
/// name, because a wrong-looking reading is indistinguishable from a real one.
/// Truncation is character-based via [`truncate_chars`], never a byte slice.
pub fn fit_label_and_value(label: &str, value: &str, has_status: bool) -> (String, String) {
    // One space between label and value, plus the status character and a space
    // before it when present.
    let reserved = value.chars().count() + 1 + if has_status { 2 } else { 0 };
    let label_max = ROW_CHARS.saturating_sub(reserved);
    (truncate_chars(label, label_max), value.to_string())
}

/// Find a sticker by DevEUI. Both sides are lowercased on config load and on
/// uplink parse, so this is a plain comparison.
fn find_sticker<'a>(
    lorawan: &'a [LoRaWANSensorState],
    dev_eui: &str,
) -> Option<&'a LoRaWANSensorState> {
    lorawan.iter().find(|s| s.dev_eui == dev_eui)
}

/// Resolve one DS18B20 line against live state.
fn build_ds18b20_line(line: &DisplayLine, ds_readings: &[Option<SensorReading>; 8]) -> (String, char, bool) {
    let reading = line
        .line
        .map(usize::from)
        .filter(|idx| *idx < ds_readings.len())
        .and_then(|idx| ds_readings[idx].as_ref());

    let alarm = reading.map(|r| r.alarm_state);
    // No reading at all means the probe slot has never reported; `?` marks
    // that apart from a probe that reported and then dropped out (`E`).
    let status = alarm.map(status_char_ds).unwrap_or('?');
    let is_alarm = matches!(alarm, Some(AlarmState::Critical));

    let value = match line.field.as_str() {
        "status" => alarm.map(alarm_text_ds).unwrap_or("----").to_string(),
        "temperature" => {
            let temp = reading
                .filter(|r| r.is_connected)
                .map(|r| f64::from(r.temperature));
            format_field_value("temperature", temp, &line.format)
        }
        _ => UNKNOWN_FIELD_VALUE.to_string(),
    };

    (value, status, is_alarm)
}

/// Resolve one sticker line against live state.
fn build_sticker_line(line: &DisplayLine, lorawan: &[LoRaWANSensorState]) -> (String, char, bool) {
    let dev_eui = line.dev_eui.as_deref().unwrap_or_default();
    let Some(sensor) = find_sticker(lorawan, dev_eui) else {
        // Configured but never seen: show the placeholder rather than dropping
        // the row, so a provisioning mistake is visible instead of invisible.
        return (placeholder_for(&line.field, &line.format), '?', false);
    };

    // Prefer the per-field alarm state so a humidity row can read NORM while a
    // temperature row on the same sticker reads CRIT.
    let field_state = sensor
        .field_alarm_states
        .get(&line.field)
        .cloned()
        .unwrap_or_else(|| sensor.alarm_state.clone());
    let disconnected = sensor.alarm_state == LoRaWANAlarmState::Disconnected
        || field_state == LoRaWANAlarmState::Disconnected;
    let effective = if disconnected {
        LoRaWANAlarmState::Disconnected
    } else {
        field_state
    };

    let value = if line.field == "status" {
        alarm_text_lora(&effective).to_string()
    } else if disconnected {
        // `LoRaWANSensorState.fields` is never cleared, so the last uplink's
        // value would otherwise sit on the panel indefinitely, looking live.
        // A dead sticker must not display a plausible temperature.
        placeholder_for(&line.field, &line.format)
    } else {
        let raw = match line.field.as_str() {
            "rssi" => sensor.rssi.map(f64::from),
            "snr" => sensor.snr.map(f64::from),
            other => match registry::lookup(other) {
                // Counters live in their own map, not in `fields`.
                Some(def) if def.kind == FieldKind::Counter => {
                    sensor.counters.get(other).map(|c| *c as f64)
                }
                Some(_) => sensor.fields.get(other).copied(),
                None => return (UNKNOWN_FIELD_VALUE.to_string(), '?', false),
            },
        };
        format_field_value(&line.field, raw, &line.format)
    };

    (
        value,
        status_char_lora(&effective),
        effective == LoRaWANAlarmState::Critical,
    )
}

/// Turn the configured lines plus live sensor state into drawable rows.
///
/// Every configured line yields exactly one row, in order — never skipped,
/// never reordered, never collapsed. A silently missing row on a medical
/// overview is worse than one showing `--.-`.
pub fn build_custom_lines(
    lines: &[DisplayLine],
    ds_readings: &[Option<SensorReading>; 8],
    ds_names: &[String; 8],
    lorawan: &[LoRaWANSensorState],
) -> Vec<RenderedLine> {
    lines
        .iter()
        .map(|line| {
            let (value, status, is_alarm) = match line.source {
                DisplayLineSource::Ds18b20 => build_ds18b20_line(line, ds_readings),
                DisplayLineSource::Sticker => build_sticker_line(line, lorawan),
            };

            let raw_label = match line.label.as_deref() {
                Some(label) => label.to_string(),
                None => default_label(line, ds_names, lorawan),
            };

            let show_status = line.format.status_char;
            let (label, value) = fit_label_and_value(&raw_label, &value, show_status);

            RenderedLine {
                label,
                value,
                status_char: show_status.then_some(status),
                is_alarm,
            }
        })
        .collect()
}

/// Lay rows out into a fixed-width character grid, mirroring where
/// `draw_custom_row` places each element.
///
/// Test-only: this is the stand-in for a real panel, since the ST7920 needs
/// GPIO. It catches column overflow and off-by-one budget errors that would
/// otherwise only show up as overlapping glyphs on hardware.
#[cfg(test)]
pub fn render_ascii(rows: &[RenderedLine]) -> String {
    rows.iter()
        .map(|row| {
            let mut cells = vec![' '; ROW_CHARS];
            for (i, c) in row.label.chars().enumerate() {
                cells[i] = c;
            }
            // Status char sits in the last column; the value's right edge is
            // two columns further left when it's present.
            let right = match row.status_char {
                Some(status) => {
                    cells[ROW_CHARS - 1] = status;
                    ROW_CHARS - 3
                }
                None => ROW_CHARS - 1,
            };
            let value: Vec<char> = row.value.chars().collect();
            let start = (right + 1).saturating_sub(value.len());
            for (i, c) in value.into_iter().enumerate() {
                if start + i < ROW_CHARS {
                    cells[start + i] = c;
                }
            }
            cells.into_iter().collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const EUI1: &str = "70b3d57ed0051f2a";
    const EUI2: &str = "70b3d57ed0051f31";

    fn fmt() -> DisplayLineFormat {
        DisplayLineFormat::default()
    }

    fn ds_line(idx: u8, field: &str) -> DisplayLine {
        DisplayLine {
            source: DisplayLineSource::Ds18b20,
            line: Some(idx),
            dev_eui: None,
            field: field.to_string(),
            label: None,
            format: fmt(),
        }
    }

    fn sticker_line(dev_eui: &str, field: &str) -> DisplayLine {
        DisplayLine {
            source: DisplayLineSource::Sticker,
            line: None,
            dev_eui: Some(dev_eui.to_string()),
            field: field.to_string(),
            label: None,
            format: fmt(),
        }
    }

    fn no_readings() -> [Option<SensorReading>; 8] {
        Default::default()
    }

    fn names() -> [String; 8] {
        std::array::from_fn(|i| format!("Probe{}", i + 1))
    }

    fn reading(temp: f32, connected: bool, alarm: AlarmState) -> Option<SensorReading> {
        Some(SensorReading {
            temperature: temp,
            is_connected: connected,
            alarm_state: alarm,
        })
    }

    /// Builder for a sticker fixture — `fields`/`counters` are generic maps, so
    /// tests have to be explicit about which map a value lands in.
    fn sticker(dev_eui: &str, name: &str, alarm: LoRaWANAlarmState) -> LoRaWANSensorState {
        LoRaWANSensorState {
            dev_eui: dev_eui.to_string(),
            name: name.to_string(),
            serial_number: None,
            location: None,
            fields: HashMap::new(),
            field_alarm_states: HashMap::new(),
            field_thresholds: Vec::new(),
            counters: HashMap::new(),
            recent_events: Default::default(),
            rssi: None,
            snr: None,
            last_seen: None,
            alarm_state: alarm,
        }
    }

    // ---- formatting -------------------------------------------------------

    #[test]
    fn format_field_value_temperature_one_decimal_with_unit() {
        assert_eq!(
            format_field_value("temperature", Some(23.44), &fmt()),
            "23.4°C"
        );
    }

    #[test]
    fn format_field_value_respects_decimals_override() {
        let f = DisplayLineFormat {
            decimals: Some(2),
            ..fmt()
        };
        assert_eq!(format_field_value("temperature", Some(23.446), &f), "23.45°C");
    }

    #[test]
    fn format_field_value_units_off_omits_suffix() {
        let f = DisplayLineFormat {
            units: false,
            ..fmt()
        };
        assert_eq!(format_field_value("temperature", Some(23.4), &f), "23.4");
        assert_eq!(format_field_value("humidity", Some(48.0), &f), "48");
    }

    #[test]
    fn format_field_value_none_yields_placeholder() {
        assert_eq!(format_field_value("temperature", None, &fmt()), "--.-°C");
        assert_eq!(format_field_value("humidity", None, &fmt()), "--%");
        assert_eq!(format_field_value("voltage", None, &fmt()), "--.--V");
    }

    #[test]
    fn placeholder_shape_is_two_dashes_plus_decimals_and_unit() {
        // Convention inherited from the built-in layout's `--.-°C`: two dashes
        // for the integer part, then the configured decimals, then the unit.
        // The width need not match the real value (`3.02V` is shorter than
        // `--.--V`) — values are right-aligned, so only their left edge moves.
        assert_eq!(placeholder_for("temperature", &fmt()), "--.-°C");
        assert_eq!(placeholder_for("humidity", &fmt()), "--%");
        assert_eq!(placeholder_for("voltage", &fmt()), "--.--V");
        assert_eq!(placeholder_for("motion_count", &fmt()), "--");
        assert_eq!(placeholder_for("status", &fmt()), "----");

        let no_units = DisplayLineFormat {
            units: false,
            ..fmt()
        };
        assert_eq!(placeholder_for("temperature", &no_units), "--.-");

        let three = DisplayLineFormat {
            decimals: Some(3),
            ..fmt()
        };
        assert_eq!(placeholder_for("temperature", &three), "--.---°C");
    }

    #[test]
    fn default_decimals_per_field() {
        assert_eq!(default_decimals("temperature"), 1);
        assert_eq!(default_decimals("ext_temperature_1"), 1);
        assert_eq!(default_decimals("humidity"), 0);
        assert_eq!(default_decimals("machine_probe_humidity_1"), 0);
        assert_eq!(default_decimals("voltage"), 2);
        assert_eq!(default_decimals("snr"), 1);
        assert_eq!(default_decimals("rssi"), 0);
        assert_eq!(default_decimals("motion_count"), 0);
    }

    #[test]
    fn unit_for_field_covers_registry_and_pseudo() {
        assert_eq!(unit_for_field("temperature"), "°C");
        assert_eq!(unit_for_field("ext_temperature_2"), "°C");
        assert_eq!(unit_for_field("machine_probe_temperature_1"), "°C");
        assert_eq!(unit_for_field("humidity"), "%");
        assert_eq!(unit_for_field("machine_probe_humidity_2"), "%");
        assert_eq!(unit_for_field("voltage"), "V");
        assert_eq!(unit_for_field("pressure"), "hPa");
        assert_eq!(unit_for_field("altitude"), "m");
        assert_eq!(unit_for_field("illuminance"), "lx");
        assert_eq!(unit_for_field("rssi"), "dBm");
        assert_eq!(unit_for_field("snr"), "dB");
        assert_eq!(unit_for_field("motion_count"), "");
        assert_eq!(unit_for_field("status"), "");
    }

    // ---- row budget -------------------------------------------------------

    #[test]
    fn fit_label_and_value_truncates_label_not_value() {
        let (label, value) = fit_label_and_value("Cold Room A Shelf 3", "1013hPa", true);
        assert_eq!(value, "1013hPa", "the value must survive intact");
        assert!(label.chars().count() <= ROW_CHARS - 10, "got {:?}", label);
        assert!(label.starts_with("Cold"));
    }

    #[test]
    fn fit_label_and_value_reserves_two_chars_for_status() {
        let (with, _) = fit_label_and_value("x".repeat(40).as_str(), "23.4°C", true);
        let (without, _) = fit_label_and_value("x".repeat(40).as_str(), "23.4°C", false);
        assert_eq!(
            without.chars().count() - with.chars().count(),
            2,
            "status char plus its gap"
        );
    }

    #[test]
    fn fit_label_and_value_never_overflows_the_row() {
        for value in ["23.4°C", "1013hPa", "-72dBm", "3.02V", "CRIT", "0"] {
            for has_status in [true, false] {
                let (label, value) = fit_label_and_value("Very Long Sensor Label", value, has_status);
                let used = label.chars().count()
                    + 1
                    + value.chars().count()
                    + if has_status { 2 } else { 0 };
                assert!(used <= ROW_CHARS, "{:?} + {:?} used {}", label, value, used);
            }
        }
    }

    #[test]
    fn fit_label_and_value_is_utf8_safe_on_multibyte_label() {
        // Regression guard for the byte-slicing bug class: a multi-byte label
        // must truncate on a character boundary, not panic mid-codepoint.
        let (label, _) = fit_label_and_value("Kühlraum Nord Süd Ost", "23.4°C", true);
        assert!(label.chars().count() <= ROW_CHARS);
        assert!(label.starts_with("Küh"));
    }

    // ---- DS18B20 lines ----------------------------------------------------

    #[test]
    fn build_lines_ds18b20_default_label_is_sensor_name() {
        let mut readings = no_readings();
        readings[2] = reading(4.5, true, AlarmState::Normal);
        let rows = build_custom_lines(&[ds_line(2, "temperature")], &readings, &names(), &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Probe3");
        assert_eq!(rows[0].value, "4.5°C");
        assert_eq!(rows[0].status_char, Some('N'));
        assert!(!rows[0].is_alarm);
    }

    #[test]
    fn build_lines_custom_label_overrides_default() {
        let mut line = ds_line(0, "temperature");
        line.label = Some("Freezer".to_string());
        let rows = build_custom_lines(&[line], &no_readings(), &names(), &[]);
        assert_eq!(rows[0].label, "Freezer");
    }

    #[test]
    fn build_lines_ds18b20_never_reported_shows_question_status() {
        let rows = build_custom_lines(&[ds_line(0, "temperature")], &no_readings(), &names(), &[]);
        assert_eq!(rows[0].value, "--.-°C");
        assert_eq!(rows[0].status_char, Some('?'));
    }

    #[test]
    fn build_lines_ds18b20_disconnected_hides_temperature() {
        let mut readings = no_readings();
        readings[0] = reading(4.5, false, AlarmState::Disconnected);
        let rows = build_custom_lines(&[ds_line(0, "temperature")], &readings, &names(), &[]);
        assert_eq!(rows[0].value, "--.-°C");
        assert_eq!(rows[0].status_char, Some('E'));
    }

    #[test]
    fn build_lines_ds18b20_critical_sets_is_alarm() {
        let mut readings = no_readings();
        readings[1] = reading(45.0, true, AlarmState::Critical);
        let rows = build_custom_lines(&[ds_line(1, "temperature")], &readings, &names(), &[]);
        assert_eq!(rows[0].status_char, Some('C'));
        assert!(rows[0].is_alarm);
    }

    #[test]
    fn build_lines_ds18b20_status_field_renders_state_text() {
        let mut readings = no_readings();
        readings[0] = reading(45.0, true, AlarmState::Critical);
        let rows = build_custom_lines(&[ds_line(0, "status")], &readings, &names(), &[]);
        assert_eq!(rows[0].value, "CRIT");
    }

    // ---- sticker lines ----------------------------------------------------

    #[test]
    fn build_lines_sticker_ext_temperature_1_resolves_from_fields_map() {
        let mut s = sticker(EUI1, "Chiller", LoRaWANAlarmState::Normal);
        s.fields.insert("ext_temperature_1".to_string(), -18.26);
        let rows = build_custom_lines(
            &[sticker_line(EUI1, "ext_temperature_1")],
            &no_readings(),
            &names(),
            &[s],
        );
        assert_eq!(rows[0].label, "Chiller");
        assert_eq!(rows[0].value, "-18.3°C");
    }

    #[test]
    fn build_lines_sticker_voltage_two_decimals_volt_unit() {
        let mut s = sticker(EUI1, "Chiller", LoRaWANAlarmState::Normal);
        s.fields.insert("voltage".to_string(), 3.02);
        let rows = build_custom_lines(
            &[sticker_line(EUI1, "voltage")],
            &no_readings(),
            &names(),
            &[s],
        );
        assert_eq!(rows[0].value, "3.02V");
    }

    #[test]
    fn build_lines_counter_field_reads_counters_map_not_fields() {
        let mut s = sticker(EUI1, "Door", LoRaWANAlarmState::Normal);
        s.counters.insert("motion_count".to_string(), 417);
        // A stray same-named entry in `fields` must not win.
        s.fields.insert("motion_count".to_string(), 1.0);
        let rows = build_custom_lines(
            &[sticker_line(EUI1, "motion_count")],
            &no_readings(),
            &names(),
            &[s],
        );
        assert_eq!(rows[0].value, "417");
    }

    #[test]
    fn build_lines_rssi_and_snr_read_from_sensor_not_fields() {
        let mut s = sticker(EUI1, "Chiller", LoRaWANAlarmState::Normal);
        s.rssi = Some(-72);
        // Not 9.25 — that's an exact tie and rounds to 9.2, which would make
        // this test about float rounding rather than about field lookup.
        s.snr = Some(9.26);
        let rows = build_custom_lines(
            &[sticker_line(EUI1, "rssi"), sticker_line(EUI1, "snr")],
            &no_readings(),
            &names(),
            &[s],
        );
        assert_eq!(rows[0].value, "-72dBm");
        assert_eq!(rows[1].value, "9.3dB");
    }

    #[test]
    fn build_lines_unknown_dev_eui_renders_placeholder_and_question_status() {
        let rows = build_custom_lines(
            &[sticker_line(EUI2, "temperature")],
            &no_readings(),
            &names(),
            &[sticker(EUI1, "Chiller", LoRaWANAlarmState::Normal)],
        );
        assert_eq!(rows[0].value, "--.-°C");
        assert_eq!(rows[0].status_char, Some('?'));
        assert!(!rows[0].is_alarm);
        // Still traceable to the sticker the user meant.
        assert_eq!(rows[0].label, "?1f31");
    }

    #[test]
    fn build_lines_missing_field_on_known_sticker_renders_placeholder() {
        // Registry field, but this sticker has no external probe attached.
        let s = sticker(EUI1, "Chiller", LoRaWANAlarmState::Normal);
        let rows = build_custom_lines(
            &[sticker_line(EUI1, "ext_temperature_1")],
            &no_readings(),
            &names(),
            &[s],
        );
        assert_eq!(rows[0].value, "--.-°C");
    }

    #[test]
    fn build_lines_disconnected_sticker_hides_stale_value() {
        // `fields` is never cleared, so without suppression this row would show
        // a plausible 22.0°C for a sticker that stopped reporting days ago.
        let mut s = sticker(EUI1, "Chiller", LoRaWANAlarmState::Disconnected);
        s.fields.insert("temperature".to_string(), 22.0);
        let rows = build_custom_lines(
            &[sticker_line(EUI1, "temperature")],
            &no_readings(),
            &names(),
            &[s],
        );
        assert_eq!(rows[0].value, "--.-°C");
        assert_eq!(rows[0].status_char, Some('E'));
    }

    #[test]
    fn build_lines_uses_field_alarm_state_not_sensor_alarm_state() {
        // Sticker is critical on temperature but fine on humidity: the humidity
        // row must not inherit the alarm.
        let mut s = sticker(EUI1, "Chiller", LoRaWANAlarmState::Critical);
        s.fields.insert("humidity".to_string(), 48.0);
        s.field_alarm_states
            .insert("humidity".to_string(), LoRaWANAlarmState::Normal);
        let rows = build_custom_lines(
            &[sticker_line(EUI1, "humidity")],
            &no_readings(),
            &names(),
            &[s],
        );
        assert_eq!(rows[0].status_char, Some('N'));
        assert!(!rows[0].is_alarm);
        assert_eq!(rows[0].value, "48%");
    }

    #[test]
    fn build_lines_critical_field_sets_is_alarm() {
        let mut s = sticker(EUI1, "Chiller", LoRaWANAlarmState::Normal);
        s.fields.insert("temperature".to_string(), 41.0);
        s.field_alarm_states
            .insert("temperature".to_string(), LoRaWANAlarmState::Critical);
        let rows = build_custom_lines(
            &[sticker_line(EUI1, "temperature")],
            &no_readings(),
            &names(),
            &[s],
        );
        assert_eq!(rows[0].status_char, Some('C'));
        assert!(rows[0].is_alarm);
    }

    #[test]
    fn build_lines_sticker_status_field_renders_state_text() {
        let s = sticker(EUI1, "Chiller", LoRaWANAlarmState::Warning);
        let rows = build_custom_lines(
            &[sticker_line(EUI1, "status")],
            &no_readings(),
            &names(),
            &[s],
        );
        assert_eq!(rows[0].value, "WARN");
    }

    #[test]
    fn build_lines_unknown_field_reads_as_misconfigured() {
        // Only reachable from a hand-edited config; the command path rejects it.
        let s = sticker(EUI1, "Chiller", LoRaWANAlarmState::Normal);
        let rows = build_custom_lines(
            &[sticker_line(EUI1, "battery_percent"), ds_line(0, "humidity")],
            &no_readings(),
            &names(),
            &[s],
        );
        assert_eq!(rows[0].value, "?");
        assert_eq!(rows[1].value, "?");
    }

    // ---- list semantics ---------------------------------------------------

    #[test]
    fn build_lines_preserves_config_order_and_length() {
        let mut s1 = sticker(EUI1, "Chiller", LoRaWANAlarmState::Normal);
        s1.fields.insert("ext_temperature_1".to_string(), -18.2);
        s1.fields.insert("voltage".to_string(), 3.02);
        let mut s2 = sticker(EUI2, "Store", LoRaWANAlarmState::Normal);
        s2.fields.insert("humidity".to_string(), 48.0);

        let mut readings = no_readings();
        readings[0] = reading(4.5, true, AlarmState::Normal);

        // Two rows on the same sticker (EUI1) — the headline use case.
        let lines = vec![
            sticker_line(EUI1, "ext_temperature_1"),
            ds_line(0, "temperature"),
            sticker_line(EUI2, "humidity"),
            sticker_line(EUI1, "voltage"),
        ];
        let rows = build_custom_lines(&lines, &readings, &names(), &[s1, s2]);

        assert_eq!(rows.len(), 4, "one row per configured line, never collapsed");
        assert_eq!(rows[0].value, "-18.2°C");
        assert_eq!(rows[1].value, "4.5°C");
        assert_eq!(rows[2].value, "48%");
        assert_eq!(rows[3].value, "3.02V");
    }

    #[test]
    fn build_lines_status_char_can_be_disabled_per_line() {
        let mut line = ds_line(0, "temperature");
        line.format.status_char = false;
        let rows = build_custom_lines(&[line], &no_readings(), &names(), &[]);
        assert_eq!(rows[0].status_char, None);
    }

    #[test]
    fn build_lines_empty_config_yields_no_rows() {
        assert!(build_custom_lines(&[], &no_readings(), &names(), &[]).is_empty());
    }

    #[test]
    fn build_lines_out_of_range_probe_index_does_not_panic() {
        // Validation rejects this on the command path, but a hand-edited file
        // can still carry it and must not take the display thread down.
        let rows = build_custom_lines(&[ds_line(200, "temperature")], &no_readings(), &names(), &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status_char, Some('?'));
    }

    // ---- layout snapshot --------------------------------------------------

    #[test]
    fn custom_overview_ascii_snapshot_matches_expected_layout() {
        let mut s1 = sticker(EUI1, "Chiller", LoRaWANAlarmState::Normal);
        s1.fields.insert("ext_temperature_1".to_string(), -18.2);
        s1.fields.insert("voltage".to_string(), 3.02);
        let mut s2 = sticker(EUI2, "Store", LoRaWANAlarmState::Normal);
        s2.fields.insert("humidity".to_string(), 48.0);

        let mut readings = no_readings();
        readings[0] = reading(4.5, true, AlarmState::Normal);

        let mut battery = sticker_line(EUI1, "voltage");
        battery.label = Some("Stkr1 bat".to_string());
        battery.format.status_char = false;

        let mut ext = sticker_line(EUI1, "ext_temperature_1");
        ext.label = Some("Stkr1 ext".to_string());
        let mut probe = ds_line(0, "temperature");
        probe.label = Some("Probe 1".to_string());
        let mut hum = sticker_line(EUI2, "humidity");
        hum.label = Some("Stkr2 RH".to_string());

        let rows = build_custom_lines(
            &[ext, probe, hum, battery],
            &readings,
            &names(),
            &[s1, s2],
        );

        // 21 columns: label left, value right-aligned before the status column.
        let expected = "\
Stkr1 ext   -18.2°C N\n\
Probe 1       4.5°C N\n\
Stkr2 RH        48% N\n\
Stkr1 bat       3.02V";
        assert_eq!(render_ascii(&rows), expected, "\n{}", render_ascii(&rows));
    }

    #[test]
    fn ascii_snapshot_shows_degraded_rows_distinctly() {
        let mut stale = sticker(EUI1, "Chiller", LoRaWANAlarmState::Disconnected);
        stale.fields.insert("temperature".to_string(), 22.0);

        let rows = build_custom_lines(
            &[
                sticker_line(EUI1, "temperature"),
                sticker_line(EUI2, "temperature"),
                ds_line(3, "temperature"),
            ],
            &no_readings(),
            &names(),
            &[stale],
        );

        let expected = "\
Chiller      --.-°C E\n\
?1f31        --.-°C ?\n\
Probe4       --.-°C ?";
        assert_eq!(render_ascii(&rows), expected, "\n{}", render_ascii(&rows));
    }
}
