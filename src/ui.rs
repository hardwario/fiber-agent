// src/ui.rs
use crate::alarms::engine::{AlarmEvent, AlarmSeverity, AlarmState};
use crate::display::St7920;
use crate::model::{ReadingQuality, SensorId, SensorReading};
use crate::power::SharedPowerStatus;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use embedded_graphics::{
    mono_font::{iso_8859_1::FONT_6X10, MonoTextStyle},
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{Line, PrimitiveStyle, Rectangle},
    text::{Alignment, Text},
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Anything that wants to be updated on each runtime tick
/// (e.g. display, web UI cache, etc.).
pub trait UiSink {
    fn on_update(
        &mut self,
        now: DateTime<Utc>,
        readings: &[SensorReading],
        alarms: &[AlarmEvent],
    );
}

/// High-level UI mode: normal overview vs service/test screens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiMode {
    Overview,
    ServiceMenu,
    SystemInfo,
    SensorDetails,
    TestMenu,
    TestAccel,
    TestClock,
    TestLed,
}

/// Items in the test menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestMenuItem {
    Accelerometer,
    Clock,
    LedTest,
}

/// System information for service menu display.
#[derive(Debug, Clone)]
pub struct SystemInfo {
    pub device_id: String,
    pub fw_version: String,
    pub ui_version: String,
    pub hw_version: String,
    pub power_percent: u8,
    pub connectivity: String,
}

impl Default for SystemInfo {
    fn default() -> Self {
        Self {
            device_id: "FIBER-001".to_string(),
            fw_version: "1.0.0".to_string(),
            ui_version: "1.0.0".to_string(),
            hw_version: "CM4".to_string(),
            power_percent: 100,
            connectivity: "WiFi".to_string(),
        }
    }
}

/// Shared handles used by the app.
pub type SharedUiMode = Arc<Mutex<UiMode>>;
pub type SharedTestSelection = Arc<Mutex<TestMenuItem>>;
pub type SharedOverviewPage = Arc<Mutex<usize>>;
pub type SharedSystemInfo = Arc<Mutex<SystemInfo>>;
pub type SharedSelectedSensorId = Arc<Mutex<Option<SensorId>>>;

/// Does nothing. Used in tests or when UI is not wired yet.
pub struct NoopUiSink;

impl UiSink for NoopUiSink {
    fn on_update(
        &mut self,
        _now: DateTime<Utc>,
        _readings: &[SensorReading],
        _alarms: &[AlarmEvent],
    ) {
        // no-op
    }
}

/// Minimal recording sink for tests / debugging.
#[derive(Debug, Default)]
pub struct RecordingUiSink {
    frames: Vec<UiFrame>,
}

#[derive(Debug, Clone)]
pub struct UiFrame {
    pub ts: DateTime<Utc>,
    pub num_readings: usize,
    pub num_alarms: usize,
}

impl RecordingUiSink {
    pub fn new() -> Self {
        Self { frames: Vec::new() }
    }

    pub fn frames(&self) -> &[UiFrame] {
        &self.frames
    }
}

impl UiSink for RecordingUiSink {
    fn on_update(
        &mut self,
        now: DateTime<Utc>,
        readings: &[SensorReading],
        alarms: &[AlarmEvent],
    ) {
        self.frames.push(UiFrame {
            ts: now,
            num_readings: readings.len(),
            num_alarms: alarms.len(),
        });
    }
}

/// Per-sensor UI status the display cares about.
#[derive(Debug, Clone)]
struct UiSensorStatus {
    last_value: Option<f32>,
    last_quality: ReadingQuality,
    last_state: AlarmState,
    last_severity: AlarmSeverity,
    last_ts: Option<DateTime<Utc>>,
}

impl Default for UiSensorStatus {
    fn default() -> Self {
        Self {
            last_value: None,
            last_quality: ReadingQuality::Ok,
            last_state: AlarmState::Normal,
            last_severity: AlarmSeverity::P3Info,
            last_ts: None,
        }
    }
}

/// Real display sink using the ST7920 and embedded-graphics.
pub struct DisplayUiSink {
    display: St7920,
    sensors: HashMap<SensorId, UiSensorStatus>,
    last_draw: Option<DateTime<Utc>>,
    min_redraw_interval: ChronoDuration,
    mode: SharedUiMode,
    test_selection: SharedTestSelection,
    overview_page: SharedOverviewPage,
    system_info: SharedSystemInfo,
    selected_sensor_id: SharedSelectedSensorId,
    power_status: SharedPowerStatus,
    /// Human-friendly labels for sensors.
    labels: Arc<HashMap<SensorId, String>>,
}

impl DisplayUiSink {
    pub fn new(
        mode: SharedUiMode,
        test_selection: SharedTestSelection,
        overview_page: SharedOverviewPage,
        system_info: SharedSystemInfo,
        selected_sensor_id: SharedSelectedSensorId,
        power_status: SharedPowerStatus,
        labels: Arc<HashMap<SensorId, String>>,
    ) -> anyhow::Result<Self> {
        let display = St7920::new()?;
        Ok(Self {
            display,
            sensors: HashMap::new(),
            last_draw: None,
            min_redraw_interval: ChronoDuration::milliseconds(50),
            mode,
            test_selection,
            overview_page,
            system_info,
            selected_sensor_id,
            power_status,
            labels,
        })
    }

    fn update_state(
        &mut self,
        now: DateTime<Utc>,
        readings: &[SensorReading],
        alarms: &[AlarmEvent],
    ) {
        // Update values from readings
        for r in readings {
            let entry = self.sensors.entry(r.sensor_id).or_default();
            entry.last_value = Some(r.value);
            entry.last_quality = r.quality;
            entry.last_ts = Some(r.ts_utc);
        }

        // Update alarm states from events
        for ev in alarms {
            let entry = self.sensors.entry(ev.sensor_id).or_default();
            entry.last_state = ev.to;
            entry.last_severity = ev.severity;
            entry.last_ts = Some(ev.ts_utc);
        }

        let _ = now;
    }

    fn redraw(&mut self, now: DateTime<Utc>) {
        self.display.clear_buffer();

        let mode = *self.mode.lock().unwrap();

        match mode {
            UiMode::Overview => self.draw_overview(now),
            UiMode::ServiceMenu => self.draw_service_menu(now),
            UiMode::SystemInfo => self.draw_system_info(now),
            UiMode::SensorDetails => self.draw_sensor_details(now),
            UiMode::TestMenu => self.draw_test_menu(now),
            UiMode::TestAccel => self.draw_test_accel(now),
            UiMode::TestClock => self.draw_test_clock(now),
            UiMode::TestLed => self.draw_test_led(now),
        }

        self.display.flush().ok();
        self.last_draw = Some(now);
    }

    /// Overview screen with up to 4 probes per page.
    /// Optimized for 128x64 pixels display.
    fn draw_overview(&mut self, now: DateTime<Utc>) {
        let text_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let header_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

        // Header: Time | Title | Page
        let time_str = now.format("%H:%M").to_string();
        Text::new(&time_str, Point::new(2, 8), text_style)
            .draw(&mut self.display)
            .ok();

        Text::with_alignment(
            "FIBER",
            Point::new(64, 8),
            header_style,
            Alignment::Center,
        )
        .draw(&mut self.display)
        .ok();

        // Header separator
        Line::new(Point::new(0, 11), Point::new(127, 11))
            .into_styled(line_style)
            .draw(&mut self.display)
            .ok();

        // Gather sensor IDs
        let mut ids: Vec<_> = self.sensors.keys().copied().collect();
        // Sort: alarms first, then by ID
        ids.sort_by(|a, b| {
            let a_alarm = self
                .sensors
                .get(a)
                .map(|s| matches!(s.last_state, AlarmState::Warning | AlarmState::Critical | AlarmState::Fault))
                .unwrap_or(false);
            let b_alarm = self
                .sensors
                .get(b)
                .map(|s| matches!(s.last_state, AlarmState::Warning | AlarmState::Critical | AlarmState::Fault))
                .unwrap_or(false);
            match (b_alarm, a_alarm) {
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                _ => a.0.cmp(&b.0),
            }
        });

        if ids.is_empty() {
            Text::with_alignment(
                "No sensors",
                Point::new(64, 36),
                text_style,
                Alignment::Center,
            )
            .draw(&mut self.display)
            .ok();
            return;
        }

        let per_page = 4usize;
        let total = ids.len();
        let max_pages = (total + per_page - 1) / per_page;

        // Page index from shared state, clamped.
        let page_idx_raw = *self.overview_page.lock().unwrap();
        let page_idx = if max_pages > 0 {
            page_idx_raw.min(max_pages - 1)
        } else {
            0
        };

        // Small "page X/Y" indicator in header
        let page_indicator = format!("{}/{}", page_idx + 1, max_pages);
        Text::with_alignment(
            &page_indicator,
            Point::new(124, 8),
            text_style,
            Alignment::Right,
        )
        .draw(&mut self.display)
        .ok();

        let start = page_idx * per_page;
        let end = usize::min(start + per_page, total);

        // Draw page indicator in header
        let page_indicator = if max_pages > 1 {
            format!("{}/{}", page_idx + 1, max_pages)
        } else {
            String::new()
        };
        if !page_indicator.is_empty() {
            Text::with_alignment(
                &page_indicator,
                Point::new(124, 8),
                text_style,
                Alignment::Right,
            )
            .draw(&mut self.display)
            .ok();
        }

        // Compact row layout: 3 probes per page to fit 128x64
        // Each probe: 1 line with label | temp | status
        for (row, sid) in ids[start..end].iter().enumerate() {
            let y = 20 + (row as i32 * 12);
            let st_opt = self.sensors.get(sid);

            let label = self
                .labels
                .get(sid)
                .cloned()
                .unwrap_or_else(|| format!("P{}", sid.0));

            let (status_char, alarmish) = if let Some(st) = st_opt {
                match st.last_state {
                    AlarmState::Normal => ("✓", false),
                    AlarmState::Warning | AlarmState::Critical | AlarmState::Fault => {
                        ("!", true)
                    }
                }
            } else {
                ("?", false)
            };

            let temp_str = if let Some(st) = st_opt {
                match st.last_quality {
                    ReadingQuality::Ok => {
                        // Show actual temperature
                        if let Some(v) = st.last_value {
                            format!("{:.1}°C", v)
                        } else {
                            "--.-°C".to_string()
                        }
                    }
                    ReadingQuality::Timeout => {
                        // Sensor initializing or temporarily unavailable
                        "INIT...".to_string()
                    }
                    _ => {
                        // Sensor not connected or other error
                        "--.-°C".to_string()
                    }
                }
            } else {
                "--.-°C".to_string()
            };

            // Highlight alarm rows
            if alarmish {
                Rectangle::new(Point::new(0, y - 8), Size::new(128, 11))
                    .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                    .draw(&mut self.display)
                    .ok();
            }

            let fg_style = if alarmish {
                MonoTextStyle::new(&FONT_6X10, BinaryColor::Off)
            } else {
                text_style
            };

            // Compact format: "Label  XX.X°  STATUS"
            Text::new(&format!("{:<8}", label), Point::new(2, y), fg_style)
                .draw(&mut self.display)
                .ok();

            Text::with_alignment(
                &temp_str,
                Point::new(70, y),
                fg_style,
                Alignment::Left,
            )
            .draw(&mut self.display)
            .ok();

            Text::with_alignment(
                status_char,
                Point::new(124, y),
                fg_style,
                Alignment::Right,
            )
            .draw(&mut self.display)
            .ok();
        }
    }

    fn draw_test_menu(&mut self, now: DateTime<Utc>) {
        let text_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let header_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

        // Header line
        Line::new(Point::new(0, 12), Point::new(127, 12))
            .into_styled(line_style)
            .draw(&mut self.display)
            .ok();

        // Title
        Text::with_alignment(
            "TEST MODE",
            Point::new(64, 8),
            header_style,
            Alignment::Center,
        )
        .draw(&mut self.display)
        .ok();

        // Time
        let time_str = now.format("%H:%M:%S").to_string();
        Text::new(&time_str, Point::new(2, 10), text_style)
            .draw(&mut self.display)
            .ok();

        let sel = *self.test_selection.lock().unwrap();

        fn draw_item(
            disp: &mut St7920,
            y: i32,
            label: &str,
            selected: bool,
            style: MonoTextStyle<BinaryColor>,
        ) {
            let prefix = if selected { ">" } else { " " };
            let line = format!("{} {}", prefix, label);
            Text::new(&line, Point::new(2, y), style)
                .draw(disp)
                .ok();
        }

        draw_item(
            &mut self.display,
            26,
            "1) Accelerometer",
            sel == TestMenuItem::Accelerometer,
            text_style,
        );
        draw_item(
            &mut self.display,
            36,
            "2) Clock",
            sel == TestMenuItem::Clock,
            text_style,
        );
        draw_item(
            &mut self.display,
            46,
            "3) LED test",
            sel == TestMenuItem::LedTest,
            text_style,
        );
    }

    fn draw_test_clock(&mut self, now: DateTime<Utc>) {
        let text_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let header_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

        Line::new(Point::new(0, 12), Point::new(127, 12))
            .into_styled(line_style)
            .draw(&mut self.display)
            .ok();

        Text::with_alignment(
            "CLOCK TEST",
            Point::new(64, 8),
            header_style,
            Alignment::Center,
        )
        .draw(&mut self.display)
        .ok();

        let date_str = now.format("%Y-%m-%d").to_string();
        let time_str = now.format("%H:%M:%S").to_string();

        Text::with_alignment(&date_str, Point::new(64, 32), text_style, Alignment::Center)
            .draw(&mut self.display)
            .ok();
        Text::with_alignment(&time_str, Point::new(64, 46), text_style, Alignment::Center)
            .draw(&mut self.display)
            .ok();
    }

    fn draw_test_accel(&mut self, now: DateTime<Utc>) {
        let text_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let header_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

        Line::new(Point::new(0, 12), Point::new(127, 12))
            .into_styled(line_style)
            .draw(&mut self.display)
            .ok();

        Text::with_alignment(
            "ACCEL TEST",
            Point::new(64, 8),
            header_style,
            Alignment::Center,
        )
        .draw(&mut self.display)
        .ok();

        let time_str = now.format("%H:%M:%S").to_string();
        Text::new(&time_str, Point::new(2, 10), text_style)
            .draw(&mut self.display)
            .ok();

        Text::new("Accelerometer demo", Point::new(2, 30), text_style)
            .draw(&mut self.display)
            .ok();
        Text::new("can be wired here", Point::new(2, 42), text_style)
            .draw(&mut self.display)
            .ok();
    }

    fn draw_test_led(&mut self, now: DateTime<Utc>) {
        let text_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let header_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

        Line::new(Point::new(0, 12), Point::new(127, 12))
            .into_styled(line_style)
            .draw(&mut self.display)
            .ok();

        Text::with_alignment(
            "LED TEST",
            Point::new(64, 8),
            header_style,
            Alignment::Center,
        )
        .draw(&mut self.display)
        .ok();

        let time_str = now.format("%H:%M:%S").to_string();
        Text::new(&time_str, Point::new(2, 10), text_style)
            .draw(&mut self.display)
            .ok();

        Text::new("Use STM test seq", Point::new(2, 30), text_style)
            .draw(&mut self.display)
            .ok();
        Text::new("to verify line LEDs", Point::new(2, 42), text_style)
            .draw(&mut self.display)
            .ok();
        Text::new("ENTER: back to menu", Point::new(2, 54), text_style)
            .draw(&mut self.display)
            .ok();
    }

    /// Service menu screen for configuration.
    fn draw_service_menu(&mut self, _now: DateTime<Utc>) {
        let text_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let header_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

        Line::new(Point::new(0, 12), Point::new(127, 12))
            .into_styled(line_style)
            .draw(&mut self.display)
            .ok();

        Text::with_alignment(
            "SERVICE MENU",
            Point::new(64, 8),
            header_style,
            Alignment::Center,
        )
        .draw(&mut self.display)
        .ok();

        Text::new("1) System Info", Point::new(2, 22), text_style)
            .draw(&mut self.display)
            .ok();
        Text::new("2) Sensor Details", Point::new(2, 32), text_style)
            .draw(&mut self.display)
            .ok();
        Text::new("3) Back to Overview", Point::new(2, 42), text_style)
            .draw(&mut self.display)
            .ok();
    }

    /// System information screen.
    fn draw_system_info(&mut self, now: DateTime<Utc>) {
        let text_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let header_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

        // Header
        Text::with_alignment(
            "SYSTEM INFO",
            Point::new(64, 8),
            header_style,
            Alignment::Center,
        )
        .draw(&mut self.display)
        .ok();

        Line::new(Point::new(0, 11), Point::new(127, 11))
            .into_styled(line_style)
            .draw(&mut self.display)
            .ok();

        let sysinfo = self.system_info.lock().unwrap();
        let power = self.power_status.lock().unwrap();
        let date_str = now.format("%d.%m").to_string();
        let time_str = now.format("%H:%M").to_string();

        // Compact layout for 128x64
        Text::new(&format!("ID: {}", sysinfo.device_id), Point::new(2, 22), text_style)
            .draw(&mut self.display)
            .ok();
        Text::new(&format!("Date: {} {}", date_str, time_str), Point::new(2, 32), text_style)
            .draw(&mut self.display)
            .ok();
        Text::new(&format!("Probes: {}", self.sensors.len()), Point::new(2, 42), text_style)
            .draw(&mut self.display)
            .ok();

        // Battery info with VBAT
        let battery_str = format!("Batt: {}% ({}mV)", power.battery_percent, power.vbat_mv);
        Text::new(&battery_str, Point::new(2, 52), text_style)
            .draw(&mut self.display)
            .ok();

        Text::new(&format!("FW: {}", sysinfo.fw_version), Point::new(2, 62), text_style)
            .draw(&mut self.display)
            .ok();
    }

    /// Sensor details screen.
    fn draw_sensor_details(&mut self, _now: DateTime<Utc>) {
        let text_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let header_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
        let line_style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

        Line::new(Point::new(0, 12), Point::new(127, 12))
            .into_styled(line_style)
            .draw(&mut self.display)
            .ok();

        Text::with_alignment(
            "SENSOR DETAILS",
            Point::new(64, 8),
            header_style,
            Alignment::Center,
        )
        .draw(&mut self.display)
        .ok();

        let selected = self.selected_sensor_id.lock().unwrap();

        if let Some(sid) = *selected {
            if let Some(st) = self.sensors.get(&sid) {
                let label = self
                    .labels
                    .get(&sid)
                    .cloned()
                    .unwrap_or_else(|| format!("Sensor {}", sid.0));

                // Show temperature based on reading quality
                let temp_str = match st.last_quality {
                    ReadingQuality::Ok => {
                        if let Some(v) = st.last_value {
                            format!("{:.1}C", v)
                        } else {
                            "--.-C".to_string()
                        }
                    }
                    ReadingQuality::Timeout => {
                        // Sensor initializing or temporarily unavailable
                        "Initializing...".to_string()
                    }
                    _ => {
                        // Sensor not connected or other error
                        "Not Connected".to_string()
                    }
                };

                let date_str = st
                    .last_ts
                    .map(|ts| ts.format("%d.%m.%Y").to_string())
                    .unwrap_or_else(|| "-.-.----".to_string());

                // Left column: Current value and thresholds
                Text::new(&label, Point::new(2, 22), text_style)
                    .draw(&mut self.display)
                    .ok();
                Text::new(&format!("Value: {}", temp_str), Point::new(2, 32), text_style)
                    .draw(&mut self.display)
                    .ok();
                Text::new(&format!("Date: {}", date_str), Point::new(2, 42), text_style)
                    .draw(&mut self.display)
                    .ok();
                Text::new("Warning: -30/-10C", Point::new(2, 52), text_style)
                    .draw(&mut self.display)
                    .ok();

                // Right column: More details
                Text::new("Alarm: -35/-5C", Point::new(64, 22), text_style)
                    .draw(&mut self.display)
                    .ok();
                Text::new("Wn Delay: 30min", Point::new(64, 32), text_style)
                    .draw(&mut self.display)
                    .ok();
                Text::new("Al Delay: 30min", Point::new(64, 42), text_style)
                    .draw(&mut self.display)
                    .ok();
                Text::new("Int: 15min", Point::new(64, 52), text_style)
                    .draw(&mut self.display)
                    .ok();

                return;
            }
        }

        Text::with_alignment(
            "No sensor selected",
            Point::new(64, 36),
            text_style,
            Alignment::Center,
        )
        .draw(&mut self.display)
        .ok();
    }
}

impl UiSink for DisplayUiSink {
    fn on_update(
        &mut self,
        now: DateTime<Utc>,
        readings: &[SensorReading],
        alarms: &[AlarmEvent],
    ) {
        self.update_state(now, readings, alarms);

        // Throttle redraw to min_redraw_interval
        let should_redraw = match self.last_draw {
            None => true,
            Some(last) => now - last >= self.min_redraw_interval,
        };

        if should_redraw {
            self.redraw(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ReadingQuality, SensorId};

    #[test]
    fn recording_ui_sink_captures_frames() {
        let mut sink = RecordingUiSink::new();
        let now = Utc::now();

        let readings = vec![SensorReading {
            ts_utc: now,
            sensor_id: SensorId(1),
            value: 4.2,
            quality: ReadingQuality::Ok,
        }];

        let alarms: Vec<AlarmEvent> = Vec::new();

        sink.on_update(now, &readings, &alarms);
        sink.on_update(now, &[], &alarms);

        let frames = sink.frames();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].num_readings, 1);
        assert_eq!(frames[1].num_readings, 0);
    }
}

// Dashboard module (Phase 4)
pub mod dashboard;
