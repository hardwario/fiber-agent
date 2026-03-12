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

/// Render the sensor overview screen showing sensors across multiple pages
/// Pages 0-1: DS18B20 sensors (4 per page), Pages 2+: LoRaWAN sensors (4 per page)
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
    lorawan_sensors: &[LoRaWANSensorState],
    total_pages: usize,
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

    // Draw header: device label centered (or "LORAWAN" for LoRaWAN pages), page/mode indicator right-aligned
    let header_label = if page >= 2 {
        "LORAWAN".to_string()
    } else if device_label.len() > 14 {
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

    if page < 2 {
        // DS18B20 pages (0-1): 4 sensors per page
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
                    AlarmState::NeverConnected => ("-", false),
                    AlarmState::Disconnected => ("E", true),
                    AlarmState::Reconnecting => ("W", true),
                    AlarmState::Normal => ("N", false),
                    AlarmState::Warning => ("W", true),
                    AlarmState::Critical => ("C", true),
                };
                (ch, is_alm)
            } else {
                ("?", false)
            };

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

            draw_sensor_row(display, y, label_x, is_selected, is_alarm, &label, &temp_str, status_char, &text_style);
        }
    } else {
        // LoRaWAN pages (2+): 4 sensors per page
        let lrw_page = page - 2;
        let start = lrw_page * 4;
        for row in 0..4 {
            let lrw_idx = start + row;
            if lrw_idx >= lorawan_sensors.len() {
                break;
            }

            let sensor = &lorawan_sensors[lrw_idx];
            let global_idx = 8 + lrw_idx; // Global sensor index
            let y = 23 + (row as i32 * 12);

            let is_selected = selected_sensor == Some(global_idx);

            // Alarm status char from overall alarm_state
            let (status_char, is_alarm) = match sensor.alarm_state {
                LoRaWANAlarmState::Normal => ("N", false),
                LoRaWANAlarmState::Warning => ("W", true),
                LoRaWANAlarmState::Critical => ("C", true),
                LoRaWANAlarmState::Disconnected => ("E", true),
            };

            // Name truncated to 6 chars to fit temp+humidity
            let name = if sensor.name.len() > 6 {
                &sensor.name[..6]
            } else {
                &sensor.name
            };

            // Format: "NAME  XX.X° XX% S"
            let temp_str = sensor.temperature
                .map(|t| format!("{:.1}", t))
                .unwrap_or_else(|| "--.-".to_string());
            let hum_str = sensor.humidity
                .map(|h| format!("{:.0}%", h))
                .unwrap_or_else(|| "--%".to_string());

            let label = format!("{:6} {}° {}", name, temp_str, hum_str);

            draw_sensor_row_wide(display, y, label_x, is_selected, is_alarm, &label, status_char, &text_style);
        }
    }

    display.flush()
}

/// Render the LoRaWAN sensor detail screen
pub fn render_lorawan_sensor_detail(
    display: &mut St7920,
    sensor: &LoRaWANSensorState,
) -> anyhow::Result<()> {
    display.clear_buffer();

    let text_style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::On);
    let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    // Header: sensor name (truncate to 16 chars)
    let display_name = if sensor.name.len() > 16 {
        &sensor.name[..16]
    } else {
        &sensor.name
    };

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
    let temp_line = format!("Temp:{} [{}]", temp_str, temp_alarm);
    Text::new(&temp_line, Point::new(2, 24), text_style)
        .draw(display)
        .ok();

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
    let hum_line = format!("Hum:{} [{}]", hum_str, hum_alarm);
    Text::new(&hum_line, Point::new(2, 37), text_style)
        .draw(display)
        .ok();

    // Line 3 (y=50): RSSI
    let rssi_str = sensor.rssi
        .map(|r| format!("{}dBm", r))
        .unwrap_or_else(|| "N/A".to_string());
    let rssi_line = format!("RSSI:{}", rssi_str);
    Text::new(&rssi_line, Point::new(2, 50), text_style)
        .draw(display)
        .ok();

    // Line 4 (y=63): Serial number or last seen
    let info_line = if let Some(ref serial) = sensor.serial_number {
        let s = if serial.len() > 18 { &serial[..18] } else { serial.as_str() };
        format!("SN:{}", s)
    } else if let Some(ref last_seen) = sensor.last_seen {
        // Show just time portion if possible
        let time_part = if last_seen.len() > 10 { &last_seen[11..] } else { last_seen.as_str() };
        let time_display = if time_part.len() > 8 { &time_part[..8] } else { time_part };
        format!("Seen:{}", time_display)
    } else {
        "No data".to_string()
    };
    Text::new(&info_line, Point::new(2, 63), text_style)
        .draw(display)
        .ok();

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
