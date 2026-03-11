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

/// Render the sensor overview screen showing all 8 sensors across 2 pages
/// When selected_sensor is Some, shows cursor at that sensor position (selection mode)
pub fn render_sensor_overview(
    display: &mut St7920,
    page: usize,
    _led_state: &SharedLedState,
    sensor_state: &SharedSensorState,
    network_status: &NetworkStatus,
    selected_sensor: Option<usize>,
    device_label: &str,
    lorawan_gateway_present: bool,
) -> anyhow::Result<()> {
    display.clear_buffer();

    let text_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::On);
    let header_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::On);
    let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    // Draw network connection icons on the left (aligned with top of FIBER text)
    let net_icon_width = icons::draw_network_status(display, 2, 0, network_status);

    // Draw LoRaWAN icon next to network icon when gateway is present
    if lorawan_gateway_present {
        icons::draw_lorawan(display, 2 + net_icon_width as i32 + 1, 0);
    }

    // Draw header: device label centered, page/mode indicator right-aligned
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
        format!("{}/2", page + 1)
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

    // Draw 4 sensors for this page
    let start_sensor = page * 4;
    for row in 0..4 {
        let sensor_idx = start_sensor + row;
        if sensor_idx >= 8 {
            break;
        }

        let y = 23 + (row as i32 * 12);

        // Check if this row is selected (cursor position)
        let is_selected = selected_sensor == Some(sensor_idx);

        // Get sensor reading to determine status from AlarmState
        let (status_char, is_alarm) = if let Some(reading) = sensor_state.readings[sensor_idx].as_ref() {
            let (ch, is_alm) = match reading.alarm_state {
                AlarmState::NeverConnected => ("-", false),   // Never connected - no alarm
                AlarmState::Disconnected => ("E", true),      // Disconnected - alarm
                AlarmState::Reconnecting => ("W", true),      // Reconnecting - show warning
                AlarmState::Normal => ("N", false),           // Normal - no alarm
                AlarmState::Warning => ("W", true),           // Warning - alarm
                AlarmState::Critical => ("C", true),          // Critical - critical
            };
            (ch, is_alm)
        } else {
            ("?", false)                                       // Unknown state
        };

        // Format sensor line: "NAME  XX.X°C  STATUS"
        // Get name from shared state (truncate to 7 chars in selection mode, 8 otherwise)
        let name = &sensor_state.names[sensor_idx];
        let max_name_len = if selected_sensor.is_some() { 7 } else { 8 };
        let label = if name.len() > max_name_len {
            format!("{}  ", &name[..max_name_len])
        } else {
            format!("{:width$}  ", name, width = max_name_len)
        };

        // Get temperature from sensor state
        let temp_str = if let Some(reading) = sensor_state.readings[sensor_idx].as_ref() {
            if reading.is_connected {
                format!("{:.1}°C", reading.temperature)
            } else {
                "--.-°C".to_string()
            }
        } else {
            "--.-°C".to_string()
        };

        // Draw background highlight for selected or alarm rows
        if is_selected {
            // Selection cursor highlight (inverted background)
            Rectangle::new(Point::new(0, y - 9), Size::new(128, 12))
                .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                .draw(display)
                .ok();

            let inverted_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::Off);

            // Draw cursor arrow
            Text::new(">", Point::new(1, y), inverted_style)
                .draw(display)
                .ok();

            Text::new(&label, Point::new(label_x, y), inverted_style)
                .draw(display)
                .ok();

            Text::with_alignment(&temp_str, Point::new(70, y), inverted_style, Alignment::Left)
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

            // Use inverted text style for alarm rows
            let inverted_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::Off);

            Text::new(&label, Point::new(label_x, y), inverted_style)
                .draw(display)
                .ok();

            Text::with_alignment(&temp_str, Point::new(70, y), inverted_style, Alignment::Left)
                .draw(display)
                .ok();

            Text::with_alignment(status_char, Point::new(126, y), inverted_style, Alignment::Right)
                .draw(display)
                .ok();
        } else {
            // Normal text style for non-alarm, non-selected rows
            Text::new(&label, Point::new(label_x, y), text_style)
                .draw(display)
                .ok();

            Text::with_alignment(&temp_str, Point::new(70, y), text_style, Alignment::Left)
                .draw(display)
                .ok();

            Text::with_alignment(status_char, Point::new(126, y), text_style, Alignment::Right)
                .draw(display)
                .ok();
        }
    }

    display.flush()
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

        eprintln!("[Screen] QR code rendered successfully");
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
    let header = format!("SYSTEM INFO {}/3", page + 1);
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

        let fw_line = format!("FW:{}", app_version);
        Text::new(&fw_line, Point::new(2, 59), text_style)
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

    display.flush()
}
