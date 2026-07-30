//! Display/UI system for FIBER Medical Thermometer
//!
//! Provides a dedicated display monitor thread that renders sensor information
//! to the ST7920 graphical LCD display. The display shows sensor temperatures,
//! alarm states, and status indicators in a multi-page format.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rppal::gpio::Gpio;

use crate::libs::leds::SharedLedStateHandle;
use crate::libs::sensors::SharedSensorStateHandle;
use crate::libs::network::{NetworkStatus, SharedProvisioningSession};
use crate::libs::lorawan::SharedLoRaWANState;
use crate::libs::buzzer::BuzzerPriorityManager;

/// Type alias for shared screen brightness handle (0-100%)
pub type SharedScreenBrightnessHandle = Arc<AtomicU8>;

/// Type alias for shared screen idle-timeout handle (seconds; 0 = always on).
/// Read live by the display loop so the timeout can be changed at runtime
/// (e.g. via MQTT) without restarting.
pub type SharedScreenTimeoutHandle = Arc<AtomicU32>;

pub mod font;
pub mod monitor;
pub mod screens;
pub mod buttons;
pub mod icons;
pub mod splash;

pub use buttons::ButtonMonitor;

/// Enum representing different display screens
#[derive(Clone, Debug)]
pub enum Screen {
    /// Sensor overview showing temperature readings
    /// page: 0 or 1 (4 sensors per page)
    /// selected_sensor: Some(0-7) when in selection mode, None when in page mode
    SensorOverview {
        page: usize,
        selected_sensor: Option<usize>,
    },
    /// Sensor detail view showing thresholds for a specific sensor
    SensorDetail { sensor_idx: usize },
    /// LoRaWAN sensor detail view
    LoRaWANSensorDetail { dev_eui: String },
    /// QR code configuration screen for Bluetooth/WiFi setup
    QrCodeConfig,
    /// System information screen with pagination
    SystemInfo { page: usize },
    /// Pairing mode - displays pairing code
    Pairing { code: String },
    /// BLE client connected — shows abbreviated client address.
    BleConnected { addr: String },
    /// BLE provisioning a WiFi connection.
    BleProvisioning { ssid: String },
    /// WiFi provisioning succeeded — auto-reverts at `until`.
    BleWifiOk { ssid: String, ip: String, until: std::time::Instant },
    /// WiFi provisioning failed — auto-reverts at `until`.
    BleWifiFail { error: String, until: std::time::Instant },
}

impl Screen {
    /// Get the current page if this is a paginated screen
    pub fn get_page(&self) -> Option<usize> {
        match self {
            Screen::SensorOverview { page, .. } => Some(*page),
            Screen::QrCodeConfig => None,
            Screen::SystemInfo { page } => Some(*page),
            Screen::Pairing { .. } => None,
            Screen::SensorDetail { .. } => None,
            Screen::LoRaWANSensorDetail { .. } => None,
            Screen::BleConnected { .. } => None,
            Screen::BleProvisioning { .. } => None,
            Screen::BleWifiOk { .. } => None,
            Screen::BleWifiFail { .. } => None,
        }
    }

    /// Check if this is a QR code screen
    pub fn is_qr_code(&self) -> bool {
        matches!(self, Screen::QrCodeConfig)
    }

    /// Check if this is a special screen (QR code or Pairing - System info allows navigation)
    pub fn is_special_screen(&self) -> bool {
        matches!(self, Screen::QrCodeConfig | Screen::Pairing { .. } | Screen::BleConnected { .. } | Screen::BleProvisioning { .. } | Screen::BleWifiOk { .. } | Screen::BleWifiFail { .. })
    }

    /// Check if this is a pairing screen
    pub fn is_pairing(&self) -> bool {
        matches!(self, Screen::Pairing { .. })
    }

    /// Check if this is a system info screen
    pub fn is_system_info(&self) -> bool {
        matches!(self, Screen::SystemInfo { .. })
    }

    /// Check if this is a navigable screen (Sensor Overview or System Info)
    pub fn is_navigable(&self) -> bool {
        matches!(self, Screen::SensorOverview { .. } | Screen::SystemInfo { .. })
    }

    /// Check if this is a sensor overview screen
    pub fn is_sensor_overview(&self) -> bool {
        matches!(self, Screen::SensorOverview { .. })
    }

    /// Check if sensor selection mode is active
    pub fn is_selection_mode(&self) -> bool {
        matches!(self, Screen::SensorOverview { selected_sensor: Some(_), .. })
    }

    /// Check if this is a sensor detail screen (DS18B20 or LoRaWAN)
    pub fn is_sensor_detail(&self) -> bool {
        matches!(self, Screen::SensorDetail { .. } | Screen::LoRaWANSensorDetail { .. })
    }

    /// Get selected sensor index if in selection mode
    pub fn get_selected_sensor(&self) -> Option<usize> {
        match self {
            Screen::SensorOverview { selected_sensor, .. } => *selected_sensor,
            _ => None,
        }
    }
}

/// Shared display state for managing screen navigation and display control
pub struct DisplayState {
    /// Current screen being displayed
    pub current_screen: Screen,
    /// Whether the display should be updated
    pub should_update: bool,
    /// Live provisioning session handle. None until [`set_provisioning_session`]
    /// is called from main. The QR-config screen pulls the current session's
    /// `QrCodeGenerator` on every frame, so rotating the session updates the
    /// rendered QR without any further plumbing.
    pub provisioning_session: Option<SharedProvisioningSession>,
    /// Current network connection status
    pub network_status: NetworkStatus,
    /// Whether a LoRaWAN gateway is present (set from main after detection)
    pub lorawan_gateway_present: bool,
    /// Shared LoRaWAN state for display rendering
    pub lorawan_state: Option<SharedLoRaWANState>,
    /// Buzzer priority manager for checking mute state
    pub buzzer_priority: Option<Arc<BuzzerPriorityManager>>,
    /// Current page within the LoRa detail view (0..3). Reset to 0 on each entry.
    pub lorawan_detail_page: u8,
    /// Shared LoRa configs handle (for rendering thresholds + location pages).
    pub lorawan_configs: Option<crate::libs::lorawan::SharedLoRaWANSensorConfigs>,
    /// Button-hold progress in pixels (0..=127). 0 = no hold active; otherwise the
    /// width of the 1-px progress bar drawn under the header divider on the sensor
    /// overview. Written by the button monitor thread each poll and read by the
    /// display monitor.
    pub hold_bar_pixels: u8,
    /// Instant of the last user activity (button press). Drives the backlight
    /// idle timeout: the display monitor turns the backlight off once the
    /// elapsed time since this exceeds the configured timeout. Written by the
    /// button monitor via [`DisplayState::mark_activity`] and read by the
    /// display monitor each frame.
    pub last_activity: Instant,
}

impl DisplayState {
    pub fn new() -> Self {
        Self {
            current_screen: Screen::SensorOverview { page: 0, selected_sensor: None },
            should_update: true,
            provisioning_session: None,
            network_status: NetworkStatus::disconnected(),
            lorawan_gateway_present: false,
            lorawan_state: None,
            buzzer_priority: None,
            lorawan_detail_page: 0,
            lorawan_configs: None,
            hold_bar_pixels: 0,
            last_activity: Instant::now(),
        }
    }

    /// Record user activity (called on any button press). Resets the backlight
    /// idle timeout so the display stays lit for another full timeout period.
    pub fn mark_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Total number of overview pages: ceil(visible_sensors / 4), never below 1.
    ///
    /// Derived from the same filtered entry list the renderer uses, so paging
    /// can't run past the last populated row. The floor of 1 keeps the `1/1`
    /// header sensible and keeps [`next_page`](Self::next_page)'s modulo safe
    /// when no sensor has ever reported.
    pub fn total_pages(
        &self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
        ds_has_reported: &[bool; 8],
    ) -> usize {
        crate::libs::display::screens::page_count(
            &self.ordered_entries(ds_readings, ds_has_reported),
        )
    }

    /// Pull the overview page and the selection cursor back into range after
    /// the visible sensor list shrinks (a LoRa sticker can leave the map), so
    /// the screen self-corrects instead of sitting on a blank page — or in
    /// selection mode with no cursor drawn — until the next button press.
    /// No-op on any other screen.
    pub fn clamp_overview(&mut self, entries: &[crate::libs::display::screens::OverviewEntry]) {
        let Screen::SensorOverview { page, selected_sensor } = &self.current_screen else {
            return;
        };
        let (page, selected_sensor) = (*page, *selected_sensor);

        // A cursor on a sensor that is no longer listed can't be drawn — move
        // it to the first surviving entry, or drop out of selection mode if
        // nothing is left.
        let (new_page, new_selected) = match selected_sensor {
            Some(idx) if !entries.iter().any(|e| e.global_idx == idx) => {
                (0, entries.first().map(|e| e.global_idx))
            }
            _ => (
                page.min(crate::libs::display::screens::page_count(entries) - 1),
                selected_sensor,
            ),
        };

        if (new_page, new_selected) != (page, selected_sensor) {
            self.current_screen = Screen::SensorOverview {
                page: new_page,
                selected_sensor: new_selected,
            };
            self.should_update = true;
        }
    }

    /// Snapshot the current ordered list of overview entries.
    /// Returns an empty list if shared state isn't ready.
    pub fn ordered_entries(
        &self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
        ds_has_reported: &[bool; 8],
    ) -> Vec<crate::libs::display::screens::OverviewEntry> {
        let lr_vec: Vec<crate::libs::lorawan::state::LoRaWANSensorState> =
            self.lorawan_state.as_ref()
                .and_then(|s| s.read().ok())
                .map(|s| {
                    let mut v: Vec<_> = s.sensors.values().cloned().collect();
                    v.sort_by(|a, b| a.dev_eui.cmp(&b.dev_eui));
                    v
                })
                .unwrap_or_default();
        crate::libs::display::screens::ordered_sensors(ds_readings, ds_has_reported, &lr_vec)
    }

    /// Get sorted LoRaWAN dev_euis for consistent indexing
    pub fn sorted_lorawan_dev_euis(&self) -> Vec<String> {
        self.lorawan_state.as_ref()
            .and_then(|s| s.read().ok())
            .map(|s| {
                let mut euis: Vec<String> = s.sensors.keys().cloned().collect();
                euis.sort();
                euis
            })
            .unwrap_or_default()
    }

    /// Attach the shared provisioning-session handle. The QR-config screen
    /// reads the inner [`crate::libs::network::ProvisioningSession`] on each
    /// frame and renders its precomputed QR.
    pub fn set_provisioning_session(&mut self, session: SharedProvisioningSession) {
        self.provisioning_session = Some(session);
    }

    /// Navigate to next page (works for sensor overview and system info when not in selection mode)
    pub fn next_page(
        &mut self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
        ds_has_reported: &[bool; 8],
    ) {
        match self.current_screen {
            Screen::SensorOverview { page, selected_sensor: None } => {
                // Dynamic page count: ceil(visible sensors / 4)
                let total = self.total_pages(ds_readings, ds_has_reported);
                self.current_screen = Screen::SensorOverview {
                    page: (page + 1) % total,
                    selected_sensor: None,
                };
            }
            Screen::SystemInfo { page } => {
                // System info has 3 pages (0, 1, 2)
                self.current_screen = Screen::SystemInfo { page: (page + 1) % 3 };
            }
            _ => {}
        }
    }

    /// Move forward through LoRa detail pages: 0 → 1 → 2, clamped at 2 (no wraparound).
    pub fn lorawan_detail_next(&mut self) {
        if matches!(self.current_screen, Screen::LoRaWANSensorDetail { .. }) {
            if self.lorawan_detail_page < 2 {
                self.lorawan_detail_page += 1;
                self.should_update = true;
            }
        }
    }

    /// Move backward through LoRa detail pages: 2 → 1 → 0, clamped at 0 (no wraparound).
    pub fn lorawan_detail_prev(&mut self) {
        if matches!(self.current_screen, Screen::LoRaWANSensorDetail { .. }) {
            if self.lorawan_detail_page > 0 {
                self.lorawan_detail_page -= 1;
                self.should_update = true;
            }
        }
    }

    /// Switch to QR code screen
    pub fn show_qr_code(&mut self) {
        self.current_screen = Screen::QrCodeConfig;
    }

    /// Return to sensor overview (page 0, no selection)
    pub fn show_sensor_overview(&mut self) {
        self.current_screen = Screen::SensorOverview { page: 0, selected_sensor: None };
    }

    /// Switch to system info screen (page 0)
    pub fn show_system_info(&mut self) {
        self.current_screen = Screen::SystemInfo { page: 0 };
    }

    /// Switch to pairing screen with code
    pub fn show_pairing(&mut self, code: String) {
        self.current_screen = Screen::Pairing { code };
        self.should_update = true;
    }

    /// Show "BLE Connected" with truncated address.
    pub fn show_ble_connected(&mut self, addr: &str) {
        let short = if addr.len() > 17 { &addr[..17] } else { addr };
        self.current_screen = Screen::BleConnected { addr: short.to_string() };
        self.should_update = true;
    }

    /// Show "Connecting WiFi..." with the SSID being attempted.
    pub fn show_ble_provisioning(&mut self, ssid: &str) {
        self.current_screen = Screen::BleProvisioning { ssid: ssid.to_string() };
        self.should_update = true;
    }

    /// Show "WiFi OK" with IP, dwell 3s, then auto-revert via tick_timed_screens.
    pub fn show_ble_wifi_ok(&mut self, ssid: &str, ip: &str) {
        self.current_screen = Screen::BleWifiOk {
            ssid: ssid.to_string(),
            ip: ip.to_string(),
            until: std::time::Instant::now() + std::time::Duration::from_secs(3),
        };
        self.should_update = true;
    }

    /// Show "WiFi Failed" with truncated error, dwell 5s.
    pub fn show_ble_wifi_fail(&mut self, error: &str) {
        let short_err: String = error.chars().take(30).collect();
        self.current_screen = Screen::BleWifiFail {
            error: short_err,
            until: std::time::Instant::now() + std::time::Duration::from_secs(5),
        };
        self.should_update = true;
    }

    /// Auto-revert from time-limited BLE provisioning screens.
    /// Called from the display monitor loop on each tick.
    pub fn tick_timed_screens(&mut self) {
        let now = std::time::Instant::now();
        let revert = match &self.current_screen {
            Screen::BleWifiOk { until, .. } | Screen::BleWifiFail { until, .. } => now >= *until,
            _ => false,
        };
        if revert {
            self.show_sensor_overview();
        }
    }

    /// Enter selection mode (from SensorOverview)
    pub fn enter_selection_mode(
        &mut self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
        ds_has_reported: &[bool; 8],
    ) {
        if let Screen::SensorOverview { page, .. } = self.current_screen {
            let entries = self.ordered_entries(ds_readings, ds_has_reported);
            if entries.is_empty() { return; }
            let pos = (page * 4).min(entries.len() - 1);
            let first_global = entries[pos].global_idx;
            self.current_screen = Screen::SensorOverview {
                page,
                selected_sensor: Some(first_global),
            };
            self.should_update = true;
        }
    }

    /// Exit selection mode (return to page mode)
    pub fn exit_selection_mode(&mut self) {
        if let Screen::SensorOverview { page, selected_sensor: Some(_) } = self.current_screen {
            self.current_screen = Screen::SensorOverview {
                page,
                selected_sensor: None,
            };
            self.should_update = true;
        }
    }

    /// Move selection cursor up within the ordered (active-first) list.
    pub fn selection_up(
        &mut self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
        ds_has_reported: &[bool; 8],
    ) {
        if let Screen::SensorOverview { selected_sensor: Some(idx), .. } = self.current_screen {
            let entries = self.ordered_entries(ds_readings, ds_has_reported);
            if entries.is_empty() { return; }
            let pos = entries.iter().position(|e| e.global_idx == idx).unwrap_or(0);
            let new_pos = if pos == 0 { entries.len() - 1 } else { pos - 1 };
            let new_global = entries[new_pos].global_idx;
            let new_page = new_pos / 4;
            self.current_screen = Screen::SensorOverview {
                page: new_page,
                selected_sensor: Some(new_global),
            };
            self.should_update = true;
        }
    }

    /// Move selection cursor down within the ordered (active-first) list.
    pub fn selection_down(
        &mut self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
        ds_has_reported: &[bool; 8],
    ) {
        if let Screen::SensorOverview { selected_sensor: Some(idx), .. } = self.current_screen {
            let entries = self.ordered_entries(ds_readings, ds_has_reported);
            if entries.is_empty() { return; }
            let pos = entries.iter().position(|e| e.global_idx == idx).unwrap_or(0);
            let new_pos = if pos + 1 >= entries.len() { 0 } else { pos + 1 };
            let new_global = entries[new_pos].global_idx;
            let new_page = new_pos / 4;
            self.current_screen = Screen::SensorOverview {
                page: new_page,
                selected_sensor: Some(new_global),
            };
            self.should_update = true;
        }
    }

    /// Enter detail view for selected sensor
    pub fn enter_detail_view(&mut self) {
        if let Screen::SensorOverview { selected_sensor: Some(idx), .. } = self.current_screen {
            if idx >= 8 {
                // LoRaWAN sensor - find dev_eui by sorted index
                let lorawan_idx = idx - 8;
                let dev_euis = self.sorted_lorawan_dev_euis();
                if let Some(dev_eui) = dev_euis.get(lorawan_idx) {
                    self.current_screen = Screen::LoRaWANSensorDetail { dev_eui: dev_eui.clone() };
                    self.lorawan_detail_page = 0;
                    self.should_update = true;
                }
            } else {
                self.current_screen = Screen::SensorDetail { sensor_idx: idx };
                self.should_update = true;
            }
        }
    }

    /// Exit detail view back to selection mode
    pub fn exit_detail_view(
        &mut self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
        ds_has_reported: &[bool; 8],
    ) {
        if !self.current_screen.is_sensor_detail() {
            return;
        }
        let target_global = match &self.current_screen {
            Screen::SensorDetail { sensor_idx } => Some(*sensor_idx),
            Screen::LoRaWANSensorDetail { dev_eui } => {
                let dev_euis = self.sorted_lorawan_dev_euis();
                dev_euis.iter().position(|e| e == dev_eui).map(|i| 8 + i)
            }
            _ => None,
        };
        // A LoRa sticker can leave the map while its detail view is open, in
        // which case there is no global index to go back to at all. Falling
        // through here would leave the detail screen up while the button state
        // machine has already moved to selection mode — the click would look
        // dead — so treat it like a sensor that is no longer listed.
        let entries = self.ordered_entries(ds_readings, ds_has_reported);
        let pos = target_global.and_then(|idx| entries.iter().position(|e| e.global_idx == idx));
        // Land on a real entry rather than storing an index the renderer can't
        // draw a cursor for, which would leave selection mode with no cursor.
        let (page, selected) = match pos {
            Some(pos) => (pos / 4, Some(entries[pos].global_idx)),
            // Nothing left to select — drop out of selection mode.
            None => (0, entries.first().map(|e| e.global_idx)),
        };
        self.current_screen = Screen::SensorOverview {
            page,
            selected_sensor: selected,
        };
        self.should_update = true;
    }
}

/// Type alias for shared display state handle
pub type SharedDisplayStateHandle = Arc<Mutex<DisplayState>>;

/// Display monitor that manages the ST7920 LCD display
///
/// This monitor runs in a dedicated thread and is responsible for:
/// - Initializing the ST7920 display hardware
/// - Reading sensor and LED state from shared handles
/// - Rendering the current screen to the display
/// - Managing page navigation
pub struct DisplayMonitor {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
    pub display_state: SharedDisplayStateHandle,
}

impl DisplayMonitor {
    /// Create and spawn the display monitor thread
    pub fn new(
        led_state: SharedLedStateHandle,
        gpio: Arc<Gpio>,
        sensor_state: SharedSensorStateHandle,
        power_status: crate::libs::power::SharedPowerStatus,
        hostname: String,
        device_label: String,
        app_version: String,
        timezone_offset_hours: i8,
        screen_brightness: SharedScreenBrightnessHandle,
        screen_timeout: SharedScreenTimeoutHandle,
    ) -> io::Result<Self> {
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();
        let display_state = Arc::new(Mutex::new(DisplayState::new()));
        let display_state_clone = display_state.clone();

        let thread_handle = thread::spawn(move || {
            monitor::display_loop(
                shutdown_flag_clone,
                display_state_clone,
                led_state,
                gpio,
                sensor_state,
                power_status,
                hostname,
                device_label,
                app_version,
                timezone_offset_hours,
                screen_brightness,
                screen_timeout,
            );
        });

        Ok(Self {
            thread_handle: Some(thread_handle),
            shutdown_flag,
            display_state,
        })
    }

    /// Set the buzzer priority manager for mute icon display.
    /// Called after both display and buzzer are initialized.
    pub fn set_buzzer_priority(&self, bp: Arc<BuzzerPriorityManager>) {
        if let Ok(mut ds) = self.display_state.lock() {
            ds.buzzer_priority = Some(bp);
        }
    }

    /// Gracefully shutdown the display monitor thread
    pub fn shutdown(mut self) -> io::Result<()> {
        // Signal the thread to shutdown
        self.shutdown_flag.store(true, Ordering::Relaxed);

        // Wait for thread to finish
        if let Some(handle) = self.thread_handle.take() {
            handle.join().ok();
        }

        Ok(())
    }
}

impl Drop for DisplayMonitor {
    fn drop(&mut self) {
        // Signal shutdown on drop
        self.shutdown_flag.store(true, Ordering::Relaxed);

        // Wait for thread with a timeout
        if let Some(handle) = self.thread_handle.take() {
            let timeout = Duration::from_secs(2);
            let start = std::time::Instant::now();
            while !handle.is_finished() && start.elapsed() < timeout {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[cfg(test)]
mod pagination_tests {
    use super::*;
    use crate::libs::alarms::AlarmState;
    use crate::libs::sensors::state::SensorReading;

    fn connected(temp: f32) -> Option<SensorReading> {
        Some(SensorReading { temperature: temp, is_connected: true, alarm_state: AlarmState::Normal })
    }

    fn empty_ds() -> [Option<SensorReading>; 8] {
        [None, None, None, None, None, None, None, None]
    }

    /// No slot has latched a connected reading yet.
    fn no_reports() -> [bool; 8] {
        [false; 8]
    }

    #[test]
    fn total_pages_is_at_least_one_when_nothing_reported() {
        let state = DisplayState::new();
        assert_eq!(state.total_pages(&empty_ds(), &no_reports()), 1);
    }

    #[test]
    fn total_pages_counts_only_visible_sensors() {
        let state = DisplayState::new();
        let mut ds_arr = empty_ds();
        for i in 0..3 { ds_arr[i] = connected(20.0); }
        assert_eq!(state.total_pages(&ds_arr, &no_reports()), 1);
        ds_arr[4] = connected(20.0);
        ds_arr[5] = connected(20.0);
        assert_eq!(state.total_pages(&ds_arr, &no_reports()), 2);
    }

    #[test]
    fn next_page_does_not_divide_by_zero_when_nothing_reported() {
        let mut state = DisplayState::new();
        state.next_page(&empty_ds(), &no_reports());
        assert!(matches!(state.current_screen, Screen::SensorOverview { page: 0, .. }));
    }

    /// `n` visible DS18B20 entries, slots 0..n.
    fn entries(n: usize) -> Vec<crate::libs::display::screens::OverviewEntry> {
        use crate::libs::display::screens::{OverviewEntry, OverviewKind};
        (0..n)
            .map(|i| OverviewEntry { kind: OverviewKind::Ds18b20, global_idx: i, active: true })
            .collect()
    }

    #[test]
    fn clamp_overview_pulls_stale_page_back() {
        let mut state = DisplayState::new();
        state.current_screen = Screen::SensorOverview { page: 3, selected_sensor: None };
        state.clamp_overview(&entries(2));
        assert!(matches!(state.current_screen, Screen::SensorOverview { page: 0, .. }));
    }

    #[test]
    fn clamp_overview_leaves_valid_page_alone() {
        let mut state = DisplayState::new();
        state.current_screen = Screen::SensorOverview { page: 1, selected_sensor: None };
        state.clamp_overview(&entries(6));
        assert!(matches!(state.current_screen, Screen::SensorOverview { page: 1, .. }));
    }

    /// A cursor left pointing at a sensor that dropped off the list (a LoRa
    /// sticker leaving the map) must move to a row the renderer can draw.
    #[test]
    fn clamp_overview_moves_cursor_off_a_vanished_sensor() {
        let mut state = DisplayState::new();
        state.current_screen = Screen::SensorOverview { page: 1, selected_sensor: Some(9) };
        state.clamp_overview(&entries(2));
        assert!(matches!(
            state.current_screen,
            Screen::SensorOverview { page: 0, selected_sensor: Some(0) }
        ));
        assert!(state.should_update);
    }

    #[test]
    fn clamp_overview_drops_selection_when_nothing_visible() {
        let mut state = DisplayState::new();
        state.current_screen = Screen::SensorOverview { page: 0, selected_sensor: Some(3) };
        state.clamp_overview(&entries(0));
        assert!(matches!(
            state.current_screen,
            Screen::SensorOverview { page: 0, selected_sensor: None }
        ));
    }

    #[test]
    fn clamp_overview_keeps_a_still_listed_cursor() {
        let mut state = DisplayState::new();
        state.current_screen = Screen::SensorOverview { page: 1, selected_sensor: Some(5) };
        state.clamp_overview(&entries(6));
        assert!(matches!(
            state.current_screen,
            Screen::SensorOverview { page: 1, selected_sensor: Some(5) }
        ));
    }

    #[test]
    fn clamp_overview_ignores_other_screens() {
        let mut state = DisplayState::new();
        state.current_screen = Screen::SystemInfo { page: 2 };
        state.clamp_overview(&entries(0));
        assert!(matches!(state.current_screen, Screen::SystemInfo { page: 2 }));
    }

    #[test]
    fn exit_detail_view_lands_on_a_listed_sensor() {
        let mut state = DisplayState::new();
        let mut ds_arr = empty_ds();
        ds_arr[4] = connected(20.0);
        // We were inspecting slot 1, which is not in the entry list.
        state.current_screen = Screen::SensorDetail { sensor_idx: 1 };
        state.exit_detail_view(&ds_arr, &no_reports());
        // Must select something the renderer can draw a cursor for, not slot 1.
        assert!(matches!(
            state.current_screen,
            Screen::SensorOverview { page: 0, selected_sensor: Some(4) }
        ));
    }

    #[test]
    fn exit_detail_view_keeps_selection_when_still_listed() {
        let mut state = DisplayState::new();
        let mut ds_arr = empty_ds();
        ds_arr[4] = connected(20.0);
        state.current_screen = Screen::SensorDetail { sensor_idx: 4 };
        state.exit_detail_view(&ds_arr, &no_reports());
        assert!(matches!(
            state.current_screen,
            Screen::SensorOverview { page: 0, selected_sensor: Some(4) }
        ));
    }

    #[test]
    fn exit_detail_view_drops_selection_when_nothing_listed() {
        let mut state = DisplayState::new();
        state.current_screen = Screen::SensorDetail { sensor_idx: 1 };
        state.exit_detail_view(&empty_ds(), &no_reports());
        assert!(matches!(
            state.current_screen,
            Screen::SensorOverview { page: 0, selected_sensor: None }
        ));
    }

    /// Regression: a sticker that leaves the map while its detail view is open
    /// has no global index left to resolve. We must still leave the detail
    /// screen — the button state machine has already moved to selection mode,
    /// so staying put makes the click look dead.
    #[test]
    fn exit_detail_view_leaves_screen_when_sticker_vanished() {
        let mut state = DisplayState::new();
        let mut ds_arr = empty_ds();
        ds_arr[4] = connected(20.0);
        state.current_screen = Screen::LoRaWANSensorDetail { dev_eui: "0011223344556677".to_string() };
        state.exit_detail_view(&ds_arr, &no_reports());
        assert!(matches!(
            state.current_screen,
            Screen::SensorOverview { page: 0, selected_sensor: Some(4) }
        ));
    }

    #[test]
    fn exit_detail_view_is_a_no_op_off_a_detail_screen() {
        let mut state = DisplayState::new();
        state.current_screen = Screen::SystemInfo { page: 1 };
        state.exit_detail_view(&empty_ds(), &no_reports());
        assert!(matches!(state.current_screen, Screen::SystemInfo { page: 1 }));
    }
}
