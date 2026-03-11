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
    /// QR code configuration screen for Bluetooth/WiFi setup
    QrCodeConfig,
    /// System information screen with pagination
    SystemInfo { page: usize },
    /// Pairing mode - displays pairing code
    Pairing { code: String },
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
        }
    }

    /// Check if this is a QR code screen
    pub fn is_qr_code(&self) -> bool {
        matches!(self, Screen::QrCodeConfig)
    }

    /// Check if this is a special screen (QR code or Pairing - System info allows navigation)
    pub fn is_special_screen(&self) -> bool {
        matches!(self, Screen::QrCodeConfig | Screen::Pairing { .. })
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

    /// Check if this is a sensor detail screen
    pub fn is_sensor_detail(&self) -> bool {
        matches!(self, Screen::SensorDetail { .. })
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
}

impl DisplayState {
    pub fn new() -> Self {
        Self {
            current_screen: Screen::SensorOverview { page: 0, selected_sensor: None },
            should_update: true,
            qr_generator: None,
            network_status: NetworkStatus::disconnected(),
            lorawan_gateway_present: false,
        }
    }

    /// Set the QR code generator
    pub fn set_qr_generator(&mut self, generator: Arc<QrCodeGenerator>) {
        self.qr_generator = Some(generator);
    }

    /// Navigate to next page (works for sensor overview and system info when not in selection mode)
    pub fn next_page(&mut self) {
        match self.current_screen {
            Screen::SensorOverview { page, selected_sensor: None } => {
                // Only page navigation when not in selection mode
                self.current_screen = Screen::SensorOverview {
                    page: if page == 0 { 1 } else { 0 },
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

    /// Enter selection mode (from SensorOverview)
    pub fn enter_selection_mode(&mut self) {
        if let Screen::SensorOverview { page, .. } = self.current_screen {
            // Start selection at first sensor on current page
            let first_sensor = page * 4;
            self.current_screen = Screen::SensorOverview {
                page,
                selected_sensor: Some(first_sensor),
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

    /// Move selection cursor up (wraps within all 8 sensors)
    pub fn selection_up(&mut self) {
        if let Screen::SensorOverview { selected_sensor: Some(idx), .. } = self.current_screen {
            let new_idx = if idx == 0 { 7 } else { idx - 1 };
            let new_page = new_idx / 4;
            self.current_screen = Screen::SensorOverview {
                page: new_page,
                selected_sensor: Some(new_idx),
            };
            self.should_update = true;
        }
    }

    /// Move selection cursor down (wraps within all 8 sensors)
    pub fn selection_down(&mut self) {
        if let Screen::SensorOverview { selected_sensor: Some(idx), .. } = self.current_screen {
            let new_idx = if idx >= 7 { 0 } else { idx + 1 };
            let new_page = new_idx / 4;
            self.current_screen = Screen::SensorOverview {
                page: new_page,
                selected_sensor: Some(new_idx),
            };
            self.should_update = true;
        }
    }

    /// Enter detail view for selected sensor
    pub fn enter_detail_view(&mut self) {
        if let Screen::SensorOverview { selected_sensor: Some(idx), .. } = self.current_screen {
            self.current_screen = Screen::SensorDetail { sensor_idx: idx };
            self.should_update = true;
        }
    }

    /// Exit detail view back to selection mode
    pub fn exit_detail_view(&mut self) {
        if let Screen::SensorDetail { sensor_idx } = self.current_screen {
            let page = sensor_idx / 4;
            self.current_screen = Screen::SensorOverview {
                page,
                selected_sensor: Some(sensor_idx),
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
