//! Screen rendering functions for display

use embedded_graphics::{
    mono_font::{iso_8859_1::FONT_6X10, MonoTextStyle},
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{Line, PrimitiveStyle, Rectangle},
    text::{Alignment, Text},
};

use crate::drivers::display::St7920;
use crate::libs::alarms::AlarmState;
use crate::libs::leds::state::SharedLedState;
use crate::libs::sensors::state::SharedSensorState;
use crate::libs::network::{QrCodeGenerator, NetworkStatus};
use crate::libs::display::icons;

/// Render the sensor overview screen showing all 8 sensors across 2 pages
pub fn render_sensor_overview(
    display: &mut St7920,
    page: usize,
    _led_state: &SharedLedState,
    sensor_state: &SharedSensorState,
    network_status: &NetworkStatus,
) -> anyhow::Result<()> {
    display.clear_buffer();

    let text_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let header_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    // Draw network connection icons on the left (aligned with top of FIBER text)
    icons::draw_network_status(display, 2, 0, network_status);

    // Draw header: "FIBER" centered, page number right-aligned
    Text::with_alignment(
        "FIBER",
        Point::new(64, 8),
        header_style,
        Alignment::Center,
    )
    .draw(display)
    .ok();

    let page_str = format!("{}/2", page + 1);
    Text::with_alignment(
        &page_str,
        Point::new(126, 8),
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

    // Draw 4 sensors for this page
    let start_sensor = page * 4;
    for row in 0..4 {
        let sensor_idx = start_sensor + row;
        if sensor_idx >= 8 {
            break;
        }

        let y = 20 + (row as i32 * 12);

        // Get sensor reading to determine status from AlarmState
        let (status_char, is_alarm) = if let Some(reading) = sensor_state.readings[sensor_idx].as_ref() {
            let (ch, is_alm) = match reading.alarm_state {
                AlarmState::NeverConnected => ("D", false),   // Never connected - no alarm
                AlarmState::Disconnected => ("E", true),      // Disconnected - alarm
                AlarmState::Reconnecting => ("W", true),      // Reconnecting - show warning
                AlarmState::Normal => ("N", false),           // Normal - no alarm
                AlarmState::Warning => ("W", true),           // Warning - alarm
                AlarmState::Alarm => ("C", true),             // Alarm - critical
                AlarmState::Critical => ("C", true),          // Critical - critical
            };
            (ch, is_alm)
        } else {
            ("?", false)                                       // Unknown state
        };

        // Format sensor line: "Sensor_  XX.X°C  STATUS"
        let label = format!("Sensor{}  ", sensor_idx);

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

        // Draw background highlight for alarm rows
        if is_alarm {
            Rectangle::new(Point::new(0, y - 8), Size::new(128, 11))
                .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                .draw(display)
                .ok();

            // Use inverted text style for alarm rows
            let inverted_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);

            Text::new(&label, Point::new(2, y), inverted_style)
                .draw(display)
                .ok();

            Text::with_alignment(&temp_str, Point::new(70, y), inverted_style, Alignment::Left)
                .draw(display)
                .ok();

            Text::with_alignment(status_char, Point::new(126, y), inverted_style, Alignment::Right)
                .draw(display)
                .ok();
        } else {
            // Normal text style for non-alarm rows
            Text::new(&label, Point::new(2, y), text_style)
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

    let text_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    // Draw title
    Text::with_alignment(
        "Scan WiFi Config",
        Point::new(64, 8),
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
        // Available space: 128px width, 52px height (from y=12 to y=62)
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
        let start_y = 14 + (available_height - qr_pixel_height) / 2;

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
            Point::new(64, 32),
            text_style,
            Alignment::Center,
        )
        .draw(display)
        .ok();
    }

    display.flush()
}
