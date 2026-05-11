//! Display/UI system for FIBER Medical Thermometer
//!
//! Provides a dedicated display monitor thread that renders sensor information
//! to the ST7920 graphical LCD display. The display shows sensor temperatures,
//! alarm states, and status indicators in a multi-page format.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rppal::gpio::Gpio;

use crate::libs::leds::SharedLedStateHandle;
use crate::libs::sensors::SharedSensorStateHandle;
use crate::libs::network::{QrCodeGenerator, NetworkStatus};
use crate::libs::lorawan::SharedLoRaWANState;
use crate::libs::buzzer::BuzzerPriorityManager;

/// Type alias for shared screen brightness handle (0-100%)
pub type SharedScreenBrightnessHandle = Arc<AtomicU8>;

pub mod font;
pub mod monitor;
pub mod screens;
pub mod buttons;
pub mod icons;

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
    /// QR code generator instance
    pub qr_generator: Option<Arc<QrCodeGenerator>>,
    /// Current network connection status
    pub network_status: NetworkStatus,
    /// Whether a LoRaWAN gateway is present (set from main after detection)
    pub lorawan_gateway_present: bool,
    /// Shared LoRaWAN state for display rendering
    pub lorawan_state: Option<SharedLoRaWANState>,
    /// Buzzer priority manager for checking mute state
    pub buzzer_priority: Option<Arc<BuzzerPriorityManager>>,
}

impl DisplayState {
    pub fn new() -> Self {
        Self {
            current_screen: Screen::SensorOverview { page: 0, selected_sensor: None },
            should_update: true,
            qr_generator: None,
            network_status: NetworkStatus::disconnected(),
            lorawan_gateway_present: false,
            lorawan_state: None,
            buzzer_priority: None,
        }
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

    /// Total number of overview pages: ceil((8 + N) / 4)
    pub fn total_pages(&self) -> usize {
        let total = self.total_sensor_count();
        (total + 3) / 4
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

    /// Set the QR code generator
    pub fn set_qr_generator(&mut self, generator: Arc<QrCodeGenerator>) {
        self.qr_generator = Some(generator);
    }

    /// Navigate to next page (works for sensor overview and system info when not in selection mode)
    pub fn next_page(&mut self) {
        match self.current_screen {
            Screen::SensorOverview { page, selected_sensor: None } => {
                // Dynamic page count: 2 DS18B20 + ceil(lorawan_count / 4)
                let total = self.total_pages();
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
    ) {
        if let Screen::SensorOverview { selected_sensor: Some(idx), .. } = self.current_screen {
            let entries = self.ordered_entries(ds_readings);
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
    ) {
        if let Screen::SensorOverview { selected_sensor: Some(idx), .. } = self.current_screen {
            let entries = self.ordered_entries(ds_readings);
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
            let page = pos / 4;
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
