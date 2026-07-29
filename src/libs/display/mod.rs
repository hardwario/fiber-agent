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

/// Type alias for the live overview-line configuration.
///
/// Read by the display loop each frame and written by the MQTT executor after a
/// successful config write, so a pushed change takes effect on the next frame
/// rather than waiting for the periodic config reconcile. Mirrors the existing
/// [`crate::libs::lorawan::SharedLoRaWANSensorConfigs`] pattern — a `Vec` can't
/// live in an atomic.
pub type SharedDisplayLinesHandle = Arc<std::sync::RwLock<Vec<crate::libs::config::DisplayLine>>>;

/// What the sensor overview is currently showing.
///
/// Derived in exactly one place ([`DisplayState::overview_mode`]) so the page
/// count, the paging arithmetic and the renderer dispatch cannot disagree about
/// which row set is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverviewMode {
    /// Built-in active-first layout: 8 DS18B20 slots plus N stickers.
    Default { rows: usize },
    /// User-configured display lines.
    ///
    /// `NonZeroUsize` is load-bearing: an empty custom list *is* [`Self::Default`]
    /// (that's the documented fallback), so "custom mode with zero rows" is a
    /// state that cannot be constructed, and no downstream arithmetic has to
    /// defend against it.
    Custom { rows: std::num::NonZeroUsize },
}

impl OverviewMode {
    /// Number of rows to page through.
    pub fn rows(&self) -> usize {
        match self {
            Self::Default { rows } => *rows,
            Self::Custom { rows } => rows.get(),
        }
    }

    /// Total number of pages. Always at least 1, for every possible input —
    /// the overview screen exists even with nothing to show on it.
    pub fn total_pages(&self) -> usize {
        self.rows().max(1).div_ceil(screens::ROWS_PER_PAGE)
    }

    /// True when the user's configured lines are being rendered.
    pub fn is_custom(&self) -> bool {
        matches!(self, Self::Custom { .. })
    }
}

/// Number of pages on the system info screen.
pub const SYSTEM_INFO_PAGES: usize = 3;

pub mod font;
pub mod monitor;
pub mod screens;
pub mod overview;
pub mod supervise;
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
    /// Number of configured custom overview lines, or 0 for the built-in
    /// layout. Refreshed by the display monitor's config reconcile; consumed by
    /// [`DisplayState::overview_mode`] so the button thread pages over the same
    /// row count the renderer is drawing.
    pub custom_line_count: usize,
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
            custom_line_count: 0,
        }
    }

    /// Record user activity (called on any button press). Resets the backlight
    /// idle timeout so the display stays lit for another full timeout period.
    pub fn mark_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Get the number of LoRaWAN sensors currently known
    pub fn lorawan_sensor_count(&self) -> usize {
        self.lorawan_state.as_ref()
            .and_then(|s| s.read().ok())
            .map(|s| s.sensors.len())
            .unwrap_or(0)
    }

    /// Total sensor count: 8 DS18B20 + N LoRaWAN
    pub fn total_sensor_count(&self) -> usize {
        8 + self.lorawan_sensor_count()
    }

    /// Which row set the overview is showing right now.
    ///
    /// Selection mode always falls back to the canonical sensor list: custom
    /// lines may repeat a source (two rows on one sticker is the headline use
    /// case), and the selection cursor is keyed on a sensor's global index, so a
    /// custom-derived list would make the second row on a sensor unreachable.
    /// Falling back also guarantees every physical sensor's detail screen stays
    /// reachable no matter how the display is configured.
    ///
    /// Total function: no `unwrap`, no panicking path.
    pub fn overview_mode(&self) -> OverviewMode {
        if self.current_screen.is_selection_mode() {
            OverviewMode::Default { rows: self.total_sensor_count() }
        } else {
            self.page_mode()
        }
    }

    /// The mode page mode would use, independent of the current screen.
    ///
    /// Needed by [`Self::exit_selection_mode`], which has to know the page count
    /// it's about to switch *to* while still in selection mode.
    fn page_mode(&self) -> OverviewMode {
        match std::num::NonZeroUsize::new(self.custom_line_count) {
            Some(rows) => OverviewMode::Custom { rows },
            None => OverviewMode::Default { rows: self.total_sensor_count() },
        }
    }

    /// Total number of overview pages for the current mode. Always >= 1.
    pub fn total_pages(&self) -> usize {
        self.overview_mode().total_pages()
    }

    /// Snapshot the current ordered list of overview entries.
    /// Returns an empty list if shared state isn't ready.
    pub fn ordered_entries(
        &self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
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
        crate::libs::display::screens::ordered_sensors(ds_readings, &lr_vec)
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
    ///
    /// Wraps with a comparison rather than `%`. This runs on the button thread,
    /// where a divide-by-zero would panic and permanently kill navigation, so
    /// there is deliberately no division here to reason about — even though
    /// [`OverviewMode::total_pages`] already guarantees a non-zero count.
    pub fn next_page(&mut self) {
        match self.current_screen {
            Screen::SensorOverview { page, selected_sensor: None } => {
                let total = self.total_pages();
                let next = if page + 1 >= total { 0 } else { page + 1 };
                self.current_screen = Screen::SensorOverview {
                    page: next,
                    selected_sensor: None,
                };
            }
            Screen::SystemInfo { page } => {
                // System info has 3 pages (0, 1, 2)
                let next = if page + 1 >= SYSTEM_INFO_PAGES { 0 } else { page + 1 };
                self.current_screen = Screen::SystemInfo { page: next };
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
    ) {
        if let Screen::SensorOverview { page, .. } = self.current_screen {
            let entries = self.ordered_entries(ds_readings);
            if entries.is_empty() { return; }
            let pos = (page * screens::ROWS_PER_PAGE).min(entries.len() - 1);
            // Derive the page from the clamped cursor rather than carrying the
            // old one over. Selection mode pages over the sensor list, which can
            // be shorter than the custom-line list we're coming from — a stale
            // page past the end would render four blank rows under a "SEL"
            // header, with the cursor sitting on a page the operator can't see.
            let page = pos / screens::ROWS_PER_PAGE;
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
            // Selection mode pages over the full sensor list, which may have
            // more pages than the custom-line list we're returning to. Without
            // this clamp the stale page would land past the end of the rows and
            // render an empty screen.
            let max_page = self.page_mode().total_pages().saturating_sub(1);
            self.current_screen = Screen::SensorOverview {
                page: page.min(max_page),
                selected_sensor: None,
            };
            self.should_update = true;
        }
    }

    /// Move selection cursor up within the ordered (active-first) list.
    pub fn selection_up(
        &mut self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
    ) {
        if let Screen::SensorOverview { selected_sensor: Some(idx), .. } = self.current_screen {
            let entries = self.ordered_entries(ds_readings);
            if entries.is_empty() { return; }
            let pos = entries.iter().position(|e| e.global_idx == idx).unwrap_or(0);
            let new_pos = if pos == 0 { entries.len() - 1 } else { pos - 1 };
            let new_global = entries[new_pos].global_idx;
            let new_page = new_pos / screens::ROWS_PER_PAGE;
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
    ) {
        if let Screen::SensorOverview { selected_sensor: Some(idx), .. } = self.current_screen {
            let entries = self.ordered_entries(ds_readings);
            if entries.is_empty() { return; }
            let pos = entries.iter().position(|e| e.global_idx == idx).unwrap_or(0);
            let new_pos = if pos + 1 >= entries.len() { 0 } else { pos + 1 };
            let new_global = entries[new_pos].global_idx;
            let new_page = new_pos / screens::ROWS_PER_PAGE;
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
    ) {
        let target_global = match &self.current_screen {
            Screen::SensorDetail { sensor_idx } => Some(*sensor_idx),
            Screen::LoRaWANSensorDetail { dev_eui } => {
                let dev_euis = self.sorted_lorawan_dev_euis();
                dev_euis.iter().position(|e| e == dev_eui).map(|i| 8 + i)
            }
            _ => None,
        };
        if let Some(idx) = target_global {
            let entries = self.ordered_entries(ds_readings);
            let pos = entries.iter().position(|e| e.global_idx == idx).unwrap_or(0);
            let page = pos / screens::ROWS_PER_PAGE;
            self.current_screen = Screen::SensorOverview {
                page,
                selected_sensor: Some(idx),
            };
            self.should_update = true;
        }
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
        display_lines: SharedDisplayLinesHandle,
    ) -> io::Result<Self> {
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();
        let display_state = Arc::new(Mutex::new(DisplayState::new()));
        let display_state_clone = display_state.clone();

        let thread_handle = thread::spawn(move || {
            // Contain panics and init failures: either one left bare would end
            // the thread for good, leaving the operator with a dark panel and no
            // indication why. Every argument is cloned per attempt so the loop is
            // re-callable, and the restarted loop re-runs St7920::init(),
            // resetting the controller out of whatever state it was left in.
            supervise::supervise("display", &shutdown_flag_clone, || {
                monitor::display_loop(
                    shutdown_flag_clone.clone(),
                    display_state_clone.clone(),
                    led_state.clone(),
                    gpio.clone(),
                    sensor_state.clone(),
                    power_status.clone(),
                    hostname.clone(),
                    device_label.clone(),
                    app_version.clone(),
                    timezone_offset_hours,
                    screen_brightness.clone(),
                    screen_timeout.clone(),
                    display_lines.clone(),
                )
            });
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
mod paging_tests {
    use super::*;

    /// A `DisplayState` with no LoRa handle attached, so `total_sensor_count()`
    /// is exactly the 8 DS18B20 slots.
    fn state(custom_line_count: usize) -> DisplayState {
        let mut ds = DisplayState::new();
        ds.custom_line_count = custom_line_count;
        ds
    }

    #[test]
    fn total_pages_uses_custom_line_count_in_page_mode() {
        assert_eq!(state(1).total_pages(), 1);
        assert_eq!(state(4).total_pages(), 1);
        assert_eq!(state(5).total_pages(), 2);
        assert_eq!(state(16).total_pages(), 4);
    }

    #[test]
    fn total_pages_uses_sensor_count_in_selection_mode() {
        let mut ds = state(1);
        // Page mode: one custom line, one page.
        assert_eq!(ds.total_pages(), 1);
        // Selection mode falls back to the canonical 8-sensor list.
        ds.current_screen = Screen::SensorOverview { page: 0, selected_sensor: Some(0) };
        assert_eq!(ds.total_pages(), 2, "8 DS18B20 slots over 4 rows/page");
    }

    #[test]
    fn total_pages_falls_back_to_sensor_count_with_no_custom_lines() {
        assert_eq!(state(0).total_pages(), 2);
    }

    #[test]
    fn overview_mode_cannot_be_custom_with_zero_rows() {
        // The invariant that makes the paging arithmetic safe by construction.
        assert!(!state(0).overview_mode().is_custom());
        assert!(state(1).overview_mode().is_custom());
    }

    #[test]
    fn total_pages_is_never_zero_over_full_input_space() {
        // Proves the invariant the paging arithmetic relies on, rather than
        // defending against a violation at each use site.
        for custom_line_count in 0..=64usize {
            for selected in [None, Some(0usize)] {
                let mut ds = state(custom_line_count);
                ds.current_screen = Screen::SensorOverview { page: 0, selected_sensor: selected };
                let total = ds.total_pages();
                assert!(
                    total >= 1,
                    "total_pages() must never be 0 (lines={}, selected={:?})",
                    custom_line_count,
                    selected,
                );

                // And next_page() must map every valid page back into range.
                for page in 0..total {
                    ds.current_screen = Screen::SensorOverview { page, selected_sensor: selected };
                    ds.next_page();
                    if selected.is_some() {
                        // Paging is disabled in selection mode.
                        assert_eq!(ds.current_screen.get_page(), Some(page));
                    } else {
                        let next = ds.current_screen.get_page().expect("still on overview");
                        assert!(next < total, "next_page() left range: {} >= {}", next, total);
                    }
                }
            }
        }
    }

    #[test]
    fn next_page_wraps_with_zero_custom_lines() {
        // Regression guard for the divide-by-zero this arithmetic used to have:
        // must land on a valid page, and above all must not panic.
        let mut ds = state(0);
        ds.current_screen = Screen::SensorOverview { page: 1, selected_sensor: None };
        ds.next_page();
        assert_eq!(ds.current_screen.get_page(), Some(0));
    }

    #[test]
    fn next_page_wraps_at_last_custom_page() {
        let mut ds = state(16);
        ds.current_screen = Screen::SensorOverview { page: 2, selected_sensor: None };
        ds.next_page();
        assert_eq!(ds.current_screen.get_page(), Some(3));
        ds.next_page();
        assert_eq!(ds.current_screen.get_page(), Some(0), "4 pages wrap 3 -> 0");
    }

    #[test]
    fn next_page_wraps_system_info_over_three_pages() {
        let mut ds = state(0);
        for expected in [1, 2, 0] {
            ds.current_screen = Screen::SystemInfo {
                page: if expected == 0 { SYSTEM_INFO_PAGES - 1 } else { expected - 1 },
            };
            ds.next_page();
            assert_eq!(ds.current_screen.get_page(), Some(expected));
        }
    }

    #[test]
    fn exit_selection_mode_clamps_stale_page() {
        // Selection mode pages over 8 sensors (2 pages); page mode here has a
        // single custom line (1 page). Without the clamp the stale page 1 would
        // render an empty screen.
        let mut ds = state(1);
        ds.current_screen = Screen::SensorOverview { page: 1, selected_sensor: Some(4) };
        ds.exit_selection_mode();
        assert_eq!(ds.current_screen.get_page(), Some(0));
        assert_eq!(ds.current_screen.get_selected_sensor(), None);
    }

    #[test]
    fn enter_selection_mode_clamps_stale_page() {
        // Mirror of exit_selection_mode_clamps_stale_page, for the entry path:
        // 16 custom lines is 4 pages, but selection mode pages over 8 sensors
        // (2 pages). Entering from custom page 3 must land on a page that
        // actually contains the cursor, not leave a blank "SEL" screen.
        let mut ds = state(16);
        ds.current_screen = Screen::SensorOverview { page: 3, selected_sensor: None };
        ds.enter_selection_mode(&Default::default());

        let page = ds.current_screen.get_page().expect("still on overview");
        let selected = ds.current_screen.get_selected_sensor().expect("cursor set");
        assert!(page < ds.total_pages(), "page {} out of {} pages", page, ds.total_pages());

        // And the cursor must be on the page being shown.
        let entries = ds.ordered_entries(&Default::default());
        let pos = entries.iter().position(|e| e.global_idx == selected).unwrap();
        assert_eq!(pos / screens::ROWS_PER_PAGE, page, "cursor is off-page");
    }

    #[test]
    fn enter_selection_mode_keeps_valid_page() {
        // No custom lines: page 1 of the 8-sensor list is valid in both modes
        // and must survive, cursor landing on the first row of that page.
        let mut ds = state(0);
        ds.current_screen = Screen::SensorOverview { page: 1, selected_sensor: None };
        ds.enter_selection_mode(&Default::default());
        assert_eq!(ds.current_screen.get_page(), Some(1));

        let selected = ds.current_screen.get_selected_sensor().expect("cursor set");
        let entries = ds.ordered_entries(&Default::default());
        let pos = entries.iter().position(|e| e.global_idx == selected).unwrap();
        assert_eq!(pos, screens::ROWS_PER_PAGE, "first row of page 1");
    }

    #[test]
    fn exit_selection_mode_keeps_valid_page() {
        let mut ds = state(16);
        ds.current_screen = Screen::SensorOverview { page: 1, selected_sensor: Some(4) };
        ds.exit_selection_mode();
        assert_eq!(ds.current_screen.get_page(), Some(1));
    }
}
