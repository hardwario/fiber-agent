//! Display monitor thread - continuously updates the ST7920 display

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rppal::gpio::Gpio;

use crate::drivers::display::St7920;
use crate::libs::leds::SharedLedStateHandle;
use crate::libs::sensors::SharedSensorStateHandle;
use crate::libs::network::get_network_status;
use crate::libs::power::SharedPowerStatus;

use super::{SharedDisplayStateHandle, Screen};
use super::screens::{render_sensor_overview, render_qr_code_screen, render_system_info, render_pairing_screen};

/// Main display loop - runs in dedicated thread
pub fn display_loop(
    shutdown_flag: Arc<AtomicBool>,
    display_state: SharedDisplayStateHandle,
    led_state: SharedLedStateHandle,
    gpio: Arc<Gpio>,
    sensor_state: SharedSensorStateHandle,
    power_status: SharedPowerStatus,
    hostname: String,
    device_label: String,
    app_version: String,
    timezone_offset_hours: i8,
) {
    // Initialize display
    let mut display = match St7920::new(gpio) {
        Ok(d) => {
            eprintln!("[DisplayMonitor] Display initialized successfully");
            d
        }
        Err(e) => {
            eprintln!("[DisplayMonitor] Failed to initialize display: {}", e);
            return;
        }
    };

    const UPDATE_INTERVAL_MS: u64 = 250; // Update display every 250ms
    let update_interval = Duration::from_millis(UPDATE_INTERVAL_MS);
    let mut last_update = std::time::Instant::now();

    eprintln!("[DisplayMonitor] Started display loop with {}ms update interval", UPDATE_INTERVAL_MS);

    // Main display loop
    loop {
        // Check for shutdown signal
        if shutdown_flag.load(Ordering::Relaxed) {
            eprintln!("[DisplayMonitor] Shutdown signal received, exiting display thread");
            break;
        }

        // Throttle updates to reduce flicker and CPU usage
        if last_update.elapsed() >= update_interval {
            last_update = std::time::Instant::now();

            // Fetch current network status
            let network_status = get_network_status();

            // Get current display state (screen and page)
            let (current_screen, qr_generator) = {
                if let Ok(mut state) = display_state.lock() {
                    // Update network status in display state
                    state.network_status = network_status.clone();
                    (state.current_screen.clone(), state.qr_generator.clone())
                } else {
                    (Screen::SensorOverview { page: 0 }, None)
                }
            };

            // Read LED state to determine sensor status
            let led_snapshot = led_state.read();

            // Dispatch rendering based on current screen
            match current_screen {
                Screen::SensorOverview { page } => {
                    // Read sensor state for temperature readings
                    let sensor_snapshot = sensor_state.read().unwrap_or_else(|_| {
                        eprintln!("[DisplayMonitor] Warning: Could not read sensor state");
                        sensor_state.read().unwrap()
                    });

                    // Render the sensor overview screen with network status
                    if let Err(e) = render_sensor_overview(&mut display, page, &led_snapshot, &sensor_snapshot, &network_status) {
                        eprintln!("[DisplayMonitor] Error rendering display: {}", e);
                    }
                }
                Screen::QrCodeConfig => {
                    // Render QR code configuration screen
                    if let Some(qr_gen) = qr_generator {
                        if let Err(e) = render_qr_code_screen(&mut display, &led_snapshot, &qr_gen) {
                            eprintln!("[DisplayMonitor] Error rendering QR code display: {}", e);
                        }
                    } else {
                        eprintln!("[DisplayMonitor] Warning: QR generator not initialized");
                    }
                }
                Screen::SystemInfo { page } => {
                    // Read sensor state for probe count
                    let sensor_snapshot = sensor_state.read().unwrap_or_else(|_| {
                        eprintln!("[DisplayMonitor] Warning: Could not read sensor state");
                        sensor_state.read().unwrap()
                    });

                    // Read power status
                    let power_snapshot = if let Ok(ps) = power_status.lock() {
                        *ps
                    } else {
                        eprintln!("[DisplayMonitor] Warning: Could not read power status");
                        crate::libs::power::PowerStatus::default()
                    };

                    // Render system info screen with page number
                    if let Err(e) = render_system_info(
                        &mut display,
                        page,
                        &sensor_snapshot,
                        &network_status,
                        &power_snapshot,
                        &hostname,
                        &device_label,
                        &app_version,
                        timezone_offset_hours,
                    ) {
                        eprintln!("[DisplayMonitor] Error rendering system info display: {}", e);
                    }
                }
                Screen::Pairing { code } => {
                    // Render pairing mode screen with code
                    if let Err(e) = render_pairing_screen(&mut display, &code) {
                        eprintln!("[DisplayMonitor] Error rendering pairing display: {}", e);
                    }
                }
            }
        }

        // Sleep to prevent busy-waiting
        thread::sleep(Duration::from_millis(50));
    }

    eprintln!("[DisplayMonitor] Display monitor thread exited cleanly");
}
