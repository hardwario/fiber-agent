//! Screen rendering functions for display

use embedded_graphics::{
    mono_font::MonoTextStyle,
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{Line, PrimitiveStyle, Rectangle},
    text::{Alignment, Text},
};
use super::font::PROFONT_9_POINT;

use std::time::UNIX_EPOCH;
use chrono::Local;

use crate::drivers::display::St7920;
use crate::libs::alarms::AlarmState;
use crate::libs::leds::state::SharedLedState;
use crate::libs::sensors::state::SharedSensorState;
use crate::libs::network::{QrCodeGenerator, NetworkStatus};
use crate::libs::display::icons;
use crate::libs::power::PowerStatus;
use crate::libs::lorawan::state::{LoRaWANSensorState, LoRaWANAlarmState};

/// A single row of the sensor overview, with its identity and active flag.
#[derive(Debug, Clone, PartialEq)]
pub struct OverviewEntry {
    /// Sensor kind (DS18B20 probe or LoRa sticker).
    pub kind: OverviewKind,
    /// Global sensor index: 0..8 for DS18B20 slot, 8..8+N for LoRa.
    pub global_idx: usize,
    /// True if the sensor has a current live reading (see `ordered_sensors`).
    pub active: bool,
}

/// Kind of sensor in the overview.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OverviewKind {
    Ds18b20,
    LoRa,
}

/// Build the overview entries with active sensors first, inactive after.
/// Within each group, underlying order is preserved (DS18B20 slot 0..7, then LoRa 0..N).
pub fn ordered_sensors(
    ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
    lorawan_sensors: &[crate::libs::lorawan::state::LoRaWANSensorState],
) -> Vec<OverviewEntry> {
    use crate::libs::lorawan::state::LoRaWANAlarmState;

    let mut all: Vec<OverviewEntry> = Vec::with_capacity(8 + lorawan_sensors.len());
    for (i, slot) in ds_readings.iter().enumerate() {
        let active = matches!(slot, Some(r) if r.is_connected);
        all.push(OverviewEntry { kind: OverviewKind::Ds18b20, global_idx: i, active });
    }
    for (i, s) in lorawan_sensors.iter().enumerate() {
        let has_reading = s.temperature.is_some() || s.humidity.is_some();
        let connected = !matches!(s.alarm_state, LoRaWANAlarmState::Disconnected);
        let active = has_reading && connected;
        all.push(OverviewEntry { kind: OverviewKind::LoRa, global_idx: 8 + i, active });
    }
    let (mut active, mut inactive): (Vec<_>, Vec<_>) = all.into_iter().partition(|e| e.active);
    active.append(&mut inactive);
    active
}

/// Render the sensor overview screen showing sensors across multiple pages.
/// Rows are rendered from the pre-computed `entries` slice (active-first ordered list).
/// When selected_sensor is Some, shows cursor at that sensor position (selection mode).
pub fn render_sensor_overview(
    display: &mut St7920,
    page: usize,
    _led_state: &SharedLedState,
    sensor_state: &SharedSensorState,
    network_status: &NetworkStatus,
    selected_sensor: Option<usize>,
    device_label: &str,
    lorawan_gateway_present: bool,
    lorawan_sensors: &[LoRaWANSensorState],
    entries: &[OverviewEntry],
    total_pages: usize,
    sensor_silenced: bool,
) -> anyhow::Result<()> {
    display.clear_buffer();

    let text_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::On);
    let header_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::On);
    let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    // Draw network connection icons on the left (aligned with top of FIBER text)
    let net_icon_width = icons::draw_network_status(display, 2, 2, network_status);

    // Draw LoRaWAN icon next to network icon when gateway is present
    if lorawan_gateway_present {
        icons::draw_lorawan(display, 2 + net_icon_width as i32 + 1, 2);
    }

    // Draw mute icon next to status icons when sensor silence is active
    if sensor_silenced {
        let mute_x = if lorawan_gateway_present {
            2 + net_icon_width as i32 + 1 + 11 + 2
        } else {
            2 + net_icon_width as i32 + 2
        };
        icons::draw_mute(display, mute_x, 3);
    }

    let header_label = if device_label.len() > 14 {
        format!("{}...", &device_label[..11])
    } else {
        device_label.to_string()
    };
    Text::with_alignment(
        &header_label,
        Point::new(64, 9),
        header_style,
        Alignment::Center,
    )
    .draw(display)
    .ok();

    // Show "SEL" when in selection mode, otherwise page number
    let mode_str = if selected_sensor.is_some() {
        "SEL".to_string()
    } else {
        format!("{}/{}", page + 1, total_pages)
    };
    Text::with_alignment(
        &mode_str,
        Point::new(126, 9),
        text_style,
        Alignment::Right,
    )
    .draw(display)
    .ok();

    // Draw horizontal separator line
    Line::new(Point::new(0, 11), Point::new(127, 11))
        .into_styled(line_style)
        .draw(display)
        .ok();

    // Calculate x offset for labels (make room for cursor in selection mode)
    let label_x = if selected_sensor.is_some() { 8 } else { 2 };

    let start = page * 4;
    let end = (start + 4).min(entries.len());
    let slice = &entries[start..end];

    for (row, entry) in slice.iter().enumerate() {
        let y = 23 + (row as i32 * 12);
        let is_selected = selected_sensor == Some(entry.global_idx);

        match entry.kind {
            OverviewKind::Ds18b20 => {
                let sensor_idx = entry.global_idx;
                let (status_char, is_alarm) = if let Some(reading) = sensor_state.readings[sensor_idx].as_ref() {
                    match reading.alarm_state {
                        AlarmState::NeverConnected => ("-", false),
                        AlarmState::Disconnected => ("E", true),
                        AlarmState::Reconnecting => ("W", true),
                        AlarmState::Normal => ("N", false),
                        AlarmState::Warning => ("W", true),
                        AlarmState::Critical => ("C", true),
                    }
                } else {
                    ("?", false)
                };
                let name = &sensor_state.names[sensor_idx];
                let max_name_len = if selected_sensor.is_some() { 7 } else { 8 };
                let label = if name.len() > max_name_len {
                    format!("{}  ", &name[..max_name_len])
                } else {
                    format!("{:width$}  ", name, width = max_name_len)
                };
                let temp_str = if let Some(reading) = sensor_state.readings[sensor_idx].as_ref() {
                    if reading.is_connected {
                        format!("{:.1}°C", reading.temperature)
                    } else {
                        "--.-°C".to_string()
                    }
                } else {
                    "--.-°C".to_string()
                };
                draw_sensor_row(display, y, label_x, is_selected, is_alarm, &label, &temp_str, status_char, &text_style);
            }
            OverviewKind::LoRa => {
                let lr_idx = entry.global_idx - 8;
                if lr_idx >= lorawan_sensors.len() { continue; }
                let sensor = &lorawan_sensors[lr_idx];
                let (status_char, is_alarm) = match sensor.alarm_state {
                    LoRaWANAlarmState::Normal => ("N", false),
                    LoRaWANAlarmState::Warning => ("W", true),
                    LoRaWANAlarmState::Critical => ("C", true),
                    LoRaWANAlarmState::Disconnected => ("E", true),
                };
                let name = if sensor.name.len() > 6 { &sensor.name[..6] } else { &sensor.name };
                let temp_str = sensor.temperature.map(|t| format!("{:.1}", t)).unwrap_or_else(|| "--.-".to_string());
                let hum_str = sensor.humidity.map(|h| format!("{:.0}%", h)).unwrap_or_else(|| "--%".to_string());
                let label = format!("{:6} {}° {}", name, temp_str, hum_str);
                draw_sensor_row_wide(display, y, label_x, is_selected, is_alarm, &label, status_char, &text_style);
            }
        }
    }

    display.flush()
}

/// Render the LoRaWAN sensor detail screen
pub fn render_lorawan_sensor_detail(
    display: &mut St7920,
    sensor: &LoRaWANSensorState,
    detail_page: u8,
    config: Option<&crate::libs::config::LoRaWANSensorConfig>,
) -> anyhow::Result<()> {
    match detail_page {
        0 => render_lorawan_detail_page_readings(display, sensor),
        1 => render_lorawan_detail_page_thresholds(display, sensor, config),
        _ => render_lorawan_detail_page_location(display, sensor, config),
    }
}

// Threshold formatting helpers
fn fmt_thresh_temp(v: Option<f32>) -> String {
    v.map(|x| format!("{:.1}", x)).unwrap_or_else(|| "--".to_string())
}

fn fmt_thresh_hum(v: Option<f32>) -> String {
    v.map(|x| format!("{:.0}", x)).unwrap_or_else(|| "--".to_string())
}

/// Wrap `text` into up to two lines of at most `width` characters each.
/// Prefers breaking on whitespace; falls back to a hard break at `width`.
/// If content overflows two lines, the second line is truncated and ends in `…`.
pub fn wrap_two_lines(text: &str, width: usize) -> [String; 2] {
    if text.is_empty() {
        return ["".to_string(), "".to_string()];
    }

    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return [text.to_string(), "".to_string()];
    }

    // Find a whitespace within [0..=width] for line 1; otherwise hard-wrap.
    let break_at = (0..=width.min(chars.len()))
        .rev()
        .find(|&i| i < chars.len() && chars[i].is_whitespace())
        .unwrap_or(width);

    let line1: String = chars[..break_at].iter().collect();
    // Skip a single whitespace at the break point if we wrapped on one.
    let rest_start = if break_at < chars.len() && chars[break_at].is_whitespace() {
        break_at + 1
    } else {
        break_at
    };
    let rest: Vec<char> = chars[rest_start..].to_vec();

    let line2 = if rest.len() <= width {
        rest.iter().collect()
    } else {
        // Overflow — truncate with ellipsis to exactly `width` chars.
        let mut s: String = rest[..width.saturating_sub(1)].iter().collect();
        s.push('…');
        s
    };

    [line1, line2]
}

fn render_lorawan_detail_header(
    display: &mut St7920,
    sensor: &LoRaWANSensorState,
    page_label: &str,
) {
    let text_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::On);
    let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    let display_name = if sensor.name.len() > 12 { &sensor.name[..12] } else { &sensor.name };

    Text::with_alignment(display_name, Point::new(64, 9), text_style, Alignment::Center)
        .draw(display).ok();

    Text::with_alignment(page_label, Point::new(126, 9), text_style, Alignment::Right)
        .draw(display).ok();

    Line::new(Point::new(0, 11), Point::new(127, 11))
        .into_styled(line_style)
        .draw(display).ok();
}

fn render_lorawan_detail_page_readings(
    display: &mut St7920,
    sensor: &LoRaWANSensorState,
) -> anyhow::Result<()> {
    display.clear_buffer();

    let text_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::On);
    render_lorawan_detail_header(display, sensor, "1/3");

    // Line 1 (y=24): Temperature and alarm state
    let temp_str = sensor.temperature
        .map(|t| format!("{:.1}C", t))
        .unwrap_or_else(|| "--.-C".to_string());
    let temp_alarm = match sensor.temp_alarm_state {
        LoRaWANAlarmState::Normal => "N",
        LoRaWANAlarmState::Warning => "W",
        LoRaWANAlarmState::Critical => "C",
        LoRaWANAlarmState::Disconnected => "E",
    };
    Text::new(&format!("Temp:{} [{}]", temp_str, temp_alarm), Point::new(2, 24), text_style)
        .draw(display).ok();

    // Line 2 (y=37): Humidity and alarm state
    let hum_str = sensor.humidity
        .map(|h| format!("{:.1}%", h))
        .unwrap_or_else(|| "--.--%".to_string());
    let hum_alarm = match sensor.humidity_alarm_state {
        LoRaWANAlarmState::Normal => "N",
        LoRaWANAlarmState::Warning => "W",
        LoRaWANAlarmState::Critical => "C",
        LoRaWANAlarmState::Disconnected => "E",
    };
    Text::new(&format!("Hum:{} [{}]", hum_str, hum_alarm), Point::new(2, 37), text_style)
        .draw(display).ok();

    // Line 3 (y=50): RSSI
    let rssi_str = sensor.rssi
        .map(|r| format!("{}dBm", r))
        .unwrap_or_else(|| "N/A".to_string());
    Text::new(&format!("RSSI:{}", rssi_str), Point::new(2, 50), text_style)
        .draw(display).ok();

    // Line 4 (y=63): Serial number or last seen
    let info_line = if let Some(ref serial) = sensor.serial_number {
        let s = if serial.len() > 18 { &serial[..18] } else { serial.as_str() };
        format!("SN:{}", s)
    } else if let Some(ref last_seen) = sensor.last_seen {
        let time_part = if last_seen.len() > 10 { &last_seen[11..] } else { last_seen.as_str() };
        let time_display = if time_part.len() > 8 { &time_part[..8] } else { time_part };
        format!("Seen:{}", time_display)
    } else {
        "No data".to_string()
    };
    Text::new(&info_line, Point::new(2, 63), text_style)
        .draw(display).ok();

    display.flush()
}

fn render_lorawan_detail_page_thresholds(
    display: &mut St7920,
    sensor: &LoRaWANSensorState,
    config: Option<&crate::libs::config::LoRaWANSensorConfig>,
) -> anyhow::Result<()> {
    display.clear_buffer();
    let text_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::On);
    render_lorawan_detail_header(display, sensor, "2/3");

    let (tcl, twl, twh, tch, hcl, hwl, hwh, hch) = match config {
        Some(c) => (
            fmt_thresh_temp(c.temp_critical_low),
            fmt_thresh_temp(c.temp_warning_low),
            fmt_thresh_temp(c.temp_warning_high),
            fmt_thresh_temp(c.temp_critical_high),
            fmt_thresh_hum(c.humidity_critical_low),
            fmt_thresh_hum(c.humidity_warning_low),
            fmt_thresh_hum(c.humidity_warning_high),
            fmt_thresh_hum(c.humidity_critical_high),
        ),
        None => (
            "--".to_string(), "--".to_string(), "--".to_string(), "--".to_string(),
            "--".to_string(), "--".to_string(), "--".to_string(), "--".to_string(),
        ),
    };

    Text::new(&format!("T crit:{} - {}", tcl, tch), Point::new(2, 24), text_style)
        .draw(display).ok();
    Text::new(&format!("T warn:{} - {}", twl, twh), Point::new(2, 37), text_style)
        .draw(display).ok();
    Text::new(&format!("H crit:{} - {}", hcl, hch), Point::new(2, 50), text_style)
        .draw(display).ok();
    Text::new(&format!("H warn:{} - {}", hwl, hwh), Point::new(2, 63), text_style)
        .draw(display).ok();

    display.flush()
}

fn render_lorawan_detail_page_location(
    display: &mut St7920,
    sensor: &LoRaWANSensorState,
    config: Option<&crate::libs::config::LoRaWANSensorConfig>,
) -> anyhow::Result<()> {
    display.clear_buffer();
    let text_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::On);
    render_lorawan_detail_header(display, sensor, "3/3");

    let location_str = config
        .and_then(|c| c.location.as_deref())
        .filter(|s| !s.is_empty());

    Text::new("Location:", Point::new(2, 24), text_style).draw(display).ok();

    match location_str {
        Some(loc) => {
            let lines = wrap_two_lines(loc, 21);
            Text::new(&lines[0], Point::new(2, 37), text_style).draw(display).ok();
            if !lines[1].is_empty() {
                Text::new(&lines[1], Point::new(2, 50), text_style).draw(display).ok();
            }
        }
        None => {
            Text::new("--", Point::new(2, 37), text_style).draw(display).ok();
        }
    }

    display.flush()
}

/// Draw a DS18B20 sensor row with label, temperature, and status
fn draw_sensor_row(
    display: &mut St7920,
    y: i32,
    label_x: i32,
    is_selected: bool,
    is_alarm: bool,
    label: &str,
    temp_str: &str,
    status_char: &str,
    text_style: &MonoTextStyle<'_, BinaryColor>,
) {
    if is_selected {
        Rectangle::new(Point::new(0, y - 9), Size::new(128, 12))
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
            .draw(display)
            .ok();

        let inverted_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::Off);

        Text::new(">", Point::new(1, y), inverted_style)
            .draw(display)
            .ok();

        Text::new(label, Point::new(label_x, y), inverted_style)
            .draw(display)
            .ok();

        Text::with_alignment(temp_str, Point::new(70, y), inverted_style, Alignment::Left)
            .draw(display)
            .ok();

        Text::with_alignment(status_char, Point::new(126, y), inverted_style, Alignment::Right)
            .draw(display)
            .ok();
    } else if is_alarm {
        Rectangle::new(Point::new(0, y - 9), Size::new(128, 12))
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
            .draw(display)
            .ok();

        let inverted_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::Off);

        Text::new(label, Point::new(label_x, y), inverted_style)
            .draw(display)
            .ok();

        Text::with_alignment(temp_str, Point::new(70, y), inverted_style, Alignment::Left)
            .draw(display)
            .ok();

        Text::with_alignment(status_char, Point::new(126, y), inverted_style, Alignment::Right)
            .draw(display)
            .ok();
    } else {
        Text::new(label, Point::new(label_x, y), *text_style)
            .draw(display)
            .ok();

        Text::with_alignment(temp_str, Point::new(70, y), *text_style, Alignment::Left)
            .draw(display)
            .ok();

        Text::with_alignment(status_char, Point::new(126, y), *text_style, Alignment::Right)
            .draw(display)
            .ok();
    }
}

/// Draw a LoRaWAN sensor row with combined label (name+temp+humidity) and status
fn draw_sensor_row_wide(
    display: &mut St7920,
    y: i32,
    label_x: i32,
    is_selected: bool,
    is_alarm: bool,
    label: &str,
    status_char: &str,
    text_style: &MonoTextStyle<'_, BinaryColor>,
) {
    if is_selected {
        Rectangle::new(Point::new(0, y - 9), Size::new(128, 12))
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
            .draw(display)
            .ok();

        let inverted_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::Off);

        Text::new(">", Point::new(1, y), inverted_style)
            .draw(display)
            .ok();

        Text::new(label, Point::new(label_x, y), inverted_style)
            .draw(display)
            .ok();

        Text::with_alignment(status_char, Point::new(126, y), inverted_style, Alignment::Right)
            .draw(display)
            .ok();
    } else if is_alarm {
        Rectangle::new(Point::new(0, y - 9), Size::new(128, 12))
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
            .draw(display)
            .ok();

        let inverted_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::Off);

        Text::new(label, Point::new(label_x, y), inverted_style)
            .draw(display)
            .ok();

        Text::with_alignment(status_char, Point::new(126, y), inverted_style, Alignment::Right)
            .draw(display)
            .ok();
    } else {
        Text::new(label, Point::new(label_x, y), *text_style)
            .draw(display)
            .ok();

        Text::with_alignment(status_char, Point::new(126, y), *text_style, Alignment::Right)
            .draw(display)
            .ok();
    }
}

/// Render the QR code configuration screen
pub fn render_qr_code_screen(
    display: &mut St7920,
    _led_state: &SharedLedState,
    qr_generator: &QrCodeGenerator,
) -> anyhow::Result<()> {
    display.clear_buffer();

    let text_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::On);
    let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    // Draw title
    Text::with_alignment(
        "Scan WiFi Config",
        Point::new(64, 9),
        text_style,
        Alignment::Center,
    )
    .draw(display)
    .ok();

    // Draw separator line
    Line::new(Point::new(0, 11), Point::new(127, 11))
        .into_styled(line_style)
        .draw(display)
        .ok();

    // Render QR code matrix
    let qr_matrix = qr_generator.get_qr_matrix();

    if !qr_matrix.is_empty() {
        let qr_size = qr_matrix.len() as i32;

        // Calculate scaling factor to fit QR on display
        // Available space: 128px width, 50px height (from y=12 to y=63)
        let available_width = 128i32;
        let available_height = 50i32;

        // Calculate scale (pixels per QR module)
        let scale_x = available_width / qr_size;
        let scale_y = available_height / qr_size;
        let scale = scale_x.min(scale_y).max(1) as u32;

        // Calculate centered position
        let qr_pixel_width = qr_size * scale as i32;
        let qr_pixel_height = qr_size * scale as i32;
        let start_x = (128 - qr_pixel_width) / 2;
        let start_y = 13 + (available_height - qr_pixel_height) / 2;

        // Draw QR code pixels
        for (row_idx, row) in qr_matrix.iter().enumerate() {
            for (col_idx, &is_black) in row.iter().enumerate() {
                if is_black {
                    let x = start_x + (col_idx as i32 * scale as i32);
                    let y = start_y + (row_idx as i32 * scale as i32);

                    // Only draw if within bounds
                    if x >= 0 && y >= 0 && x < 128 && y < 64 {
                        Rectangle::new(
                            Point::new(x, y),
                            Size::new(scale, scale),
                        )
                        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                        .draw(display)
                        .ok();
                    }
                }
            }
        }

    } else {
        eprintln!("[Screen] Warning: QR matrix is empty!");
        Text::with_alignment(
            "QR Error",
            Point::new(64, 38),
            text_style,
            Alignment::Center,
        )
        .draw(display)
        .ok();
    }

    display.flush()
}

/// Render the system information screen with pagination (3 pages)
pub fn render_system_info(
    display: &mut St7920,
    page: usize,
    sensor_state: &SharedSensorState,
    network_status: &NetworkStatus,
    power_status: &PowerStatus,
    hostname: &str,
    device_label: &str,
    app_version: &str,
) -> anyhow::Result<()> {
    display.clear_buffer();

    let text_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::On);
    let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    // Header: "SYSTEM INFO" centered with page indicator
    let header = if cfg!(feature = "dev-platform") {
        format!("DEV INFO {}/3", page + 1)
    } else {
        format!("SYSTEM INFO {}/3", page + 1)
    };
    Text::with_alignment(
        &header,
        Point::new(64, 9),
        text_style,
        Alignment::Center,
    )
    .draw(display)
    .ok();

    // Separator line
    Line::new(Point::new(0, 11), Point::new(127, 11))
        .into_styled(line_style)
        .draw(display)
        .ok();

    // Data lines at 12px spacing: y=23, 35, 47, 59
    if page == 0 {
        // PAGE 1: Basic System Info

        let id_line = format!("ID:{}", hostname);
        Text::new(&id_line, Point::new(2, 23), text_style)
            .draw(display)
            .ok();

        let date_str = format!("Date:{}", format_date());
        Text::new(&date_str, Point::new(2, 35), text_style)
            .draw(display)
            .ok();

        let time_str = format!("Time:{}", format_time());
        Text::new(&time_str, Point::new(2, 47), text_style)
            .draw(display)
            .ok();

        let connected = count_connected_probes(sensor_state);
        let power_str = if power_status.on_dc_power { "PoE" } else { "Bat" };
        let status_line = format!("Probes:{}/8 PWR:{}", connected, power_str);
        Text::new(&status_line, Point::new(2, 59), text_style)
            .draw(display)
            .ok();

    } else if page == 1 {
        // PAGE 2: Network & Power Info

        let wifi = if network_status.wifi_connected { "On" } else { "Off" };
        let wifi_line = format!("WiFi:{}", wifi);
        Text::new(&wifi_line, Point::new(2, 23), text_style)
            .draw(display)
            .ok();

        let eth = if network_status.ethernet_connected { "On" } else { "Off" };
        let eth_line = format!("Ethernet:{}", eth);
        Text::new(&eth_line, Point::new(2, 35), text_style)
            .draw(display)
            .ok();

        let battery_line = format!("Battery:{}%", power_status.battery_percent);
        Text::new(&battery_line, Point::new(2, 47), text_style)
            .draw(display)
            .ok();

        let alarm_str = format_last_alarm(power_status);
        let alarm_line = format!("LastAlarm:{}", alarm_str);
        Text::new(&alarm_line, Point::new(2, 59), text_style)
            .draw(display)
            .ok();

    } else {
        // PAGE 3: Device Label, IPs & Version

        let label_display = if device_label.len() > 18 {
            format!("{}...", &device_label[..15])
        } else {
            device_label.to_string()
        };
        let label_line = format!("Label:{}", label_display);
        Text::new(&label_line, Point::new(2, 23), text_style)
            .draw(display)
            .ok();

        let wifi_ip = network_status.wifi_ip.as_deref().unwrap_or("N/A");
        let wifi_ip_line = format!("WiFi:{}", wifi_ip);
        Text::new(&wifi_ip_line, Point::new(2, 35), text_style)
            .draw(display)
            .ok();

        let eth_ip = network_status.ethernet_ip.as_deref().unwrap_or("N/A");
        let eth_ip_line = format!("Eth:{}", eth_ip);
        Text::new(&eth_ip_line, Point::new(2, 47), text_style)
            .draw(display)
            .ok();

        let fw_version = if cfg!(feature = "dev-platform") {
            format!("FW:{}-dev", app_version)
        } else {
            format!("FW:{}", app_version)
        };
        Text::new(&fw_version, Point::new(2, 59), text_style)
            .draw(display)
            .ok();
    }

    display.flush()
}

/// Format current date as YYYY-MM-DD using Linux system local time
fn format_date() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

/// Format current time as HH:MM:SS using Linux system local time
fn format_time() -> String {
    Local::now().format("%H:%M:%S").to_string()
}

/// Count number of connected probes
fn count_connected_probes(sensor_state: &SharedSensorState) -> usize {
    sensor_state.readings.iter()
        .filter(|r| r.as_ref().map_or(false, |reading| reading.is_connected))
        .count()
}

/// Format last power alarm timestamp using Linux system local time
fn format_last_alarm(power_status: &PowerStatus) -> String {
    if let Some(alarm_time) = power_status.last_dc_loss_time {
        let duration = alarm_time.duration_since(UNIX_EPOCH).unwrap_or_default();
        let dt = chrono::DateTime::from_timestamp(duration.as_secs() as i64, 0)
            .map(|utc| utc.with_timezone(&chrono::Local));
        match dt {
            Some(local) => local.format("%H:%M:%S").to_string(),
            None => "Error".to_string(),
        }
    } else {
        "None".to_string()
    }
}

/// Render the pairing mode screen showing the pairing code
pub fn render_pairing_screen(
    display: &mut St7920,
    code: &str,
) -> anyhow::Result<()> {
    display.clear_buffer();

    let text_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::On);
    let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    // Draw title
    Text::with_alignment(
        "PAIRING MODE",
        Point::new(64, 9),
        text_style,
        Alignment::Center,
    )
    .draw(display)
    .ok();

    // Draw separator line
    Line::new(Point::new(0, 11), Point::new(127, 11))
        .into_styled(line_style)
        .draw(display)
        .ok();

    // Draw instruction text
    Text::with_alignment(
        "Enter code in Viewer:",
        Point::new(64, 25),
        text_style,
        Alignment::Center,
    )
    .draw(display)
    .ok();

    // Draw the pairing code prominently (larger space between chars)
    let spaced_code: String = code.chars()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" ");

    Text::with_alignment(
        &spaced_code,
        Point::new(64, 40),
        text_style,
        Alignment::Center,
    )
    .draw(display)
    .ok();

    // Draw box around the code for emphasis
    Rectangle::new(Point::new(20, 29), Size::new(88, 16))
        .into_styled(line_style)
        .draw(display)
        .ok();

    // Draw expiry hint
    Text::with_alignment(
        "Code expires in 5 min",
        Point::new(64, 57),
        text_style,
        Alignment::Center,
    )
    .draw(display)
    .ok();

    display.flush()
}

/// Render the sensor detail screen showing thresholds and current reading
pub fn render_sensor_detail(
    display: &mut St7920,
    sensor_idx: usize,
    sensor_state: &SharedSensorState,
) -> anyhow::Result<()> {
    display.clear_buffer();

    let text_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::On);
    let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    // Get sensor name (truncate to 16 chars for display)
    let name = &sensor_state.names[sensor_idx];
    let display_name = if name.len() > 16 {
        &name[..16]
    } else {
        name.as_str()
    };

    // Header: Sensor name centered
    Text::with_alignment(
        display_name,
        Point::new(64, 9),
        text_style,
        Alignment::Center,
    )
    .draw(display)
    .ok();

    // Separator line
    Line::new(Point::new(0, 11), Point::new(127, 11))
        .into_styled(line_style)
        .draw(display)
        .ok();

    // Get current reading and thresholds
    let reading = sensor_state.readings[sensor_idx].as_ref();
    let threshold = &sensor_state.thresholds[sensor_idx];

    // Line 1 (y=24): Current temperature and alarm state
    let (temp_str, state_str) = if let Some(r) = reading {
        let temp = if r.is_connected {
            format!("{:.1}C", r.temperature)
        } else {
            "--.-C".to_string()
        };
        let state = match r.alarm_state {
            AlarmState::NeverConnected => "-",
            AlarmState::Disconnected => "E",
            AlarmState::Reconnecting => "W",
            AlarmState::Normal => "N",
            AlarmState::Warning => "W",
            AlarmState::Critical => "C",
        };
        (temp, state)
    } else {
        ("--.-C".to_string(), "?")
    };
    let now_line = format!("Now:{} [{}]", temp_str, state_str);
    Text::new(&now_line, Point::new(2, 24), text_style)
        .draw(display)
        .ok();

    // Line 2 (y=37): Critical thresholds
    let critical_line = format!("CL:{:.1} CH:{:.1}", threshold.critical_low_celsius, threshold.critical_high_celsius);
    Text::new(&critical_line, Point::new(2, 37), text_style)
        .draw(display)
        .ok();

    // Line 3 (y=50): Warning thresholds
    let warning_line = format!("WL:{:.1} WH:{:.1}", threshold.warning_low_celsius, threshold.warning_high_celsius);
    Text::new(&warning_line, Point::new(2, 50), text_style)
        .draw(display)
        .ok();

    // Line 4 (y=63): Location (if set)
    if let Some(location) = sensor_state.get_location(sensor_idx as u8) {
        if !location.is_empty() {
            let loc_line = format!("Loc:{}", if location.len() > 12 { &location[..12] } else { location });
            Text::new(&loc_line, Point::new(2, 63), text_style)
                .draw(display)
                .ok();
        }
    }

    display.flush()
}

/// Render the BLE-connected screen (transient — shows when a BLE client is connected).
pub fn render_ble_connected(
    display: &mut St7920,
    addr: &str,
) -> anyhow::Result<()> {
    display.clear_buffer();

    let text_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::On);
    let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    Text::with_alignment("BLE Connected", Point::new(64, 18), text_style, Alignment::Center)
        .draw(display).ok();

    Line::new(Point::new(0, 22), Point::new(127, 22))
        .into_styled(line_style)
        .draw(display).ok();

    Text::with_alignment(addr, Point::new(64, 42), text_style, Alignment::Center)
        .draw(display).ok();

    display.flush()
}

/// Render the WiFi-provisioning screen (during connect attempt).
pub fn render_ble_provisioning(
    display: &mut St7920,
    ssid: &str,
) -> anyhow::Result<()> {
    display.clear_buffer();

    let text_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::On);
    let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    Text::with_alignment("Connecting WiFi...", Point::new(64, 18), text_style, Alignment::Center)
        .draw(display).ok();

    Line::new(Point::new(0, 22), Point::new(127, 22))
        .into_styled(line_style)
        .draw(display).ok();

    Text::with_alignment(ssid, Point::new(64, 42), text_style, Alignment::Center)
        .draw(display).ok();

    display.flush()
}

/// Render the "WiFi OK" success screen.
pub fn render_ble_wifi_ok(
    display: &mut St7920,
    ssid: &str,
    ip: &str,
) -> anyhow::Result<()> {
    display.clear_buffer();

    let text_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::On);
    let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    Text::with_alignment("WiFi OK", Point::new(64, 18), text_style, Alignment::Center)
        .draw(display).ok();

    Line::new(Point::new(0, 22), Point::new(127, 22))
        .into_styled(line_style)
        .draw(display).ok();

    Text::with_alignment(ssid, Point::new(64, 38), text_style, Alignment::Center)
        .draw(display).ok();

    Text::with_alignment(ip, Point::new(64, 54), text_style, Alignment::Center)
        .draw(display).ok();

    display.flush()
}

/// Render the "WiFi Failed" error screen.
pub fn render_ble_wifi_fail(
    display: &mut St7920,
    error: &str,
) -> anyhow::Result<()> {
    display.clear_buffer();

    let text_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::On);
    let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    Text::with_alignment("WiFi Failed", Point::new(64, 18), text_style, Alignment::Center)
        .draw(display).ok();

    Line::new(Point::new(0, 22), Point::new(127, 22))
        .into_styled(line_style)
        .draw(display).ok();

    Text::with_alignment(error, Point::new(64, 42), text_style, Alignment::Center)
        .draw(display).ok();

    display.flush()
}

#[cfg(test)]
mod ordering_tests {
    use super::*;
    use crate::libs::alarms::AlarmState;
    use crate::libs::sensors::state::SensorReading;
    use crate::libs::lorawan::state::{LoRaWANSensorState, LoRaWANAlarmState};

    fn ds(temp: f32, connected: bool) -> Option<SensorReading> {
        Some(SensorReading {
            temperature: temp,
            is_connected: connected,
            alarm_state: if connected { AlarmState::Normal } else { AlarmState::Disconnected },
        })
    }

    fn lora(name: &str, temp: Option<f32>, alarm: LoRaWANAlarmState) -> LoRaWANSensorState {
        LoRaWANSensorState {
            dev_eui: name.to_string(),
            name: name.to_string(),
            serial_number: None,
            temperature: temp,
            humidity: None,
            voltage: None,
            ext_temperature_1: None,
            ext_temperature_2: None,
            illuminance: None,
            motion_count: None,
            orientation: None,
            rssi: None,
            snr: None,
            last_seen: None,
            alarm_state: alarm.clone(),
            temp_alarm_state: alarm.clone(),
            humidity_alarm_state: alarm,
        }
    }

    fn empty_ds() -> [Option<SensorReading>; 8] {
        [None, None, None, None, None, None, None, None]
    }

    #[test]
    fn all_inactive_keeps_underlying_order() {
        let ds_arr = empty_ds();
        let lr = vec![lora("a", None, LoRaWANAlarmState::Disconnected),
                      lora("b", None, LoRaWANAlarmState::Disconnected)];
        let entries = ordered_sensors(&ds_arr, &lr);
        assert_eq!(entries.len(), 10);
        for (i, e) in entries.iter().enumerate() {
            assert_eq!(e.global_idx, i);
            assert!(!e.active);
        }
    }

    #[test]
    fn all_active_keeps_underlying_order() {
        let mut ds_arr = empty_ds();
        for i in 0..8 { ds_arr[i] = ds(20.0, true); }
        let lr = vec![lora("a", Some(21.0), LoRaWANAlarmState::Normal)];
        let entries = ordered_sensors(&ds_arr, &lr);
        assert_eq!(entries.len(), 9);
        for (i, e) in entries.iter().enumerate() {
            assert_eq!(e.global_idx, i);
            assert!(e.active);
        }
    }

    #[test]
    fn mixed_active_first_then_inactive() {
        let mut ds_arr = empty_ds();
        ds_arr[1] = ds(20.0, true);
        ds_arr[5] = ds(20.0, true);
        let lr = vec![
            lora("a", None, LoRaWANAlarmState::Disconnected),
            lora("b", Some(21.0), LoRaWANAlarmState::Normal),
        ];
        let entries = ordered_sensors(&ds_arr, &lr);
        let global_indices: Vec<usize> = entries.iter().map(|e| e.global_idx).collect();
        assert_eq!(global_indices, vec![1, 5, 9, 0, 2, 3, 4, 6, 7, 8]);
        assert!(entries[0].active);
        assert!(entries[1].active);
        assert!(entries[2].active);
        assert!(!entries[3].active);
    }

    #[test]
    fn lora_disconnected_with_temperature_still_inactive() {
        let ds_arr = empty_ds();
        let lr = vec![lora("a", Some(20.0), LoRaWANAlarmState::Disconnected)];
        let entries = ordered_sensors(&ds_arr, &lr);
        assert!(!entries[0].active);
    }

    #[test]
    fn ds_disconnected_is_inactive_even_with_reading() {
        let mut ds_arr = empty_ds();
        ds_arr[0] = Some(SensorReading {
            temperature: 20.0,
            is_connected: false,
            alarm_state: AlarmState::Disconnected,
        });
        let lr = vec![];
        let entries = ordered_sensors(&ds_arr, &lr);
        assert!(!entries[0].active);
    }
}

#[cfg(test)]
mod wrap_tests {
    use super::*;

    #[test]
    fn empty_returns_empty_lines() {
        let lines = wrap_two_lines("", 21);
        assert_eq!(lines, ["".to_string(), "".to_string()]);
    }

    #[test]
    fn short_fits_on_first_line() {
        let lines = wrap_two_lines("Cold room A", 21);
        assert_eq!(lines, ["Cold room A".to_string(), "".to_string()]);
    }

    #[test]
    fn wraps_on_whitespace() {
        let lines = wrap_two_lines("Cold room A, shelf 3 corner", 21);
        // "Cold room A, shelf 3" is 20 chars (fits in 21); rest goes to line 2
        assert_eq!(lines[0], "Cold room A, shelf 3");
        assert_eq!(lines[1], "corner");
    }

    #[test]
    fn hard_wraps_when_no_whitespace() {
        let lines = wrap_two_lines("AAAAAAAAAAAAAAAAAAAAABBBBBB", 21);
        assert_eq!(lines[0], "AAAAAAAAAAAAAAAAAAAAA");
        assert_eq!(lines[1], "BBBBBB");
    }

    #[test]
    fn truncates_with_ellipsis_when_overflow() {
        let lines = wrap_two_lines(
            "AAAAAAAAAAAAAAAAAAAAABBBBBBBBBBBBBBBBBBBBBCCCCCC",
            21,
        );
        assert_eq!(lines[0], "AAAAAAAAAAAAAAAAAAAAA");
        assert!(lines[1].ends_with('…'));
        assert_eq!(lines[1].chars().count(), 21);
    }
}
