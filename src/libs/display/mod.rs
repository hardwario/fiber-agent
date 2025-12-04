//! Display/UI system for FIBER Medical Thermometer
//!
//! Provides a dedicated display monitor thread that renders sensor information
//! to the ST7920 graphical LCD display. The display shows sensor temperatures,
//! alarm states, and status indicators in a multi-page format.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rppal::gpio::Gpio;

use crate::libs::leds::SharedLedStateHandle;
use crate::libs::sensors::SharedSensorStateHandle;
use crate::libs::network::{QrCodeGenerator, NetworkStatus};

pub mod monitor;
pub mod screens;
pub mod buttons;
pub mod icons;

pub use buttons::ButtonMonitor;

/// Enum representing different display screens
#[derive(Clone, Debug)]
pub enum Screen {
    /// Sensor overview showing temperature readings
    SensorOverview { page: usize },
    /// QR code configuration screen for Bluetooth/WiFi setup
    QrCodeConfig,
}

impl Screen {
    /// Get the current page if this is a sensor overview screen
    pub fn get_page(&self) -> Option<usize> {
        match self {
            Screen::SensorOverview { page } => Some(*page),
            Screen::QrCodeConfig => None,
        }
    }

    /// Check if this is a QR code screen
    pub fn is_qr_code(&self) -> bool {
        matches!(self, Screen::QrCodeConfig)
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
}

impl DisplayState {
    pub fn new() -> Self {
        Self {
            current_screen: Screen::SensorOverview { page: 0 },
            should_update: true,
            qr_generator: None,
            network_status: NetworkStatus::disconnected(),
        }
    }

    /// Set the QR code generator
    pub fn set_qr_generator(&mut self, generator: Arc<QrCodeGenerator>) {
        self.qr_generator = Some(generator);
    }

    /// Navigate to next page (only for sensor overview)
    pub fn next_page(&mut self) {
        if let Screen::SensorOverview { page } = self.current_screen {
            self.current_screen = Screen::SensorOverview { page: if page == 0 { 1 } else { 0 } };
        }
    }

    /// Switch to QR code screen
    pub fn show_qr_code(&mut self) {
        self.current_screen = Screen::QrCodeConfig;
    }

    /// Return to sensor overview (last page)
    pub fn show_sensor_overview(&mut self) {
        self.current_screen = Screen::SensorOverview { page: 0 };
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
    pub fn new(led_state: SharedLedStateHandle, gpio: Arc<Gpio>, sensor_state: SharedSensorStateHandle) -> io::Result<Self> {
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();
        let display_state = Arc::new(Mutex::new(DisplayState::new()));
        let display_state_clone = display_state.clone();

        let thread_handle = thread::spawn(move || {
            monitor::display_loop(shutdown_flag_clone, display_state_clone, led_state, gpio, sensor_state);
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
