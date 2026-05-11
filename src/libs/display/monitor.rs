//! Display monitor thread - continuously updates the ST7920 display

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rppal::gpio::Gpio;

use crate::drivers::display::St7920;
use crate::libs::leds::SharedLedStateHandle;
use crate::libs::sensors::SharedSensorStateHandle;
use crate::libs::network::get_network_status;
use crate::libs::power::SharedPowerStatus;
use crate::libs::lorawan::LoRaWANSensorState;

use super::{SharedDisplayStateHandle, Screen};
use super::screens::{
    render_sensor_overview, render_qr_code_screen, render_system_info,
    render_pairing_screen, render_sensor_detail, render_lorawan_sensor_detail,
    render_ble_connected, render_ble_provisioning, render_ble_wifi_ok, render_ble_wifi_fail,
};

/// Main display loop - runs in dedicated thread
pub fn display_loop(
    shutdown_flag: Arc<AtomicBool>,
    display_state: SharedDisplayStateHandle,
    led_state: SharedLedStateHandle,
    gpio: Arc<Gpio>,
    sensor_state: SharedSensorStateHandle,
    power_status: SharedPowerStatus,
    hostname: String,
    _device_label: String,
    app_version: String,
    _timezone_offset_hours: i8,
    screen_brightness: Arc<AtomicU8>,
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

    // Track last applied brightness to detect changes
    let mut last_brightness: u8 = 100; // Default to full brightness

    eprintln!("[DisplayMonitor] Started display loop with {}ms update interval", UPDATE_INTERVAL_MS);

    // Main display loop
    loop {
        // Check for shutdown signal
        if shutdown_flag.load(Ordering::Relaxed) {
            eprintln!("[DisplayMonitor] Shutdown signal received, exiting display thread");
            break;
        }

        // Check for brightness changes and apply via PWM
        let current_brightness = screen_brightness.load(Ordering::Relaxed);
        if current_brightness != last_brightness {
            if let Err(e) = display.set_brightness(current_brightness) {
                eprintln!("[DisplayMonitor] Failed to set brightness to {}%: {}", current_brightness, e);
            } else {
                eprintln!("[DisplayMonitor] Screen brightness set to {}%", current_brightness);
            }
            last_brightness = current_brightness;
        }

        // Throttle updates to reduce flicker and CPU usage
        if last_update.elapsed() >= update_interval {
            last_update = std::time::Instant::now();

            // Fetch current network status
            let network_status = get_network_status();

            // Get current display state (screen and page)
            let (current_screen, qr_generator, lorawan_gateway_present, total_pages) = {
                if let Ok(mut state) = display_state.lock() {
                    // Revert any expired timed screens (BleWifiOk / BleWifiFail) before rendering
                    state.tick_timed_screens();
                    // Update network status in display state
                    state.network_status = network_status.clone();
                    let tp = state.total_pages();
                    (state.current_screen.clone(), state.qr_generator.clone(), state.lorawan_gateway_present, tp)
                } else {
                    (Screen::SensorOverview { page: 0, selected_sensor: None }, None, false, 2)
                }
            };

            // Read LED state to determine sensor status
            let led_snapshot = led_state.read();

            // Dispatch rendering based on current screen
            match current_screen {
                Screen::SensorOverview { page, selected_sensor } => {
                    // Read sensor state for temperature readings
                    let sensor_snapshot = sensor_state.read().unwrap_or_else(|_| {
                        eprintln!("[DisplayMonitor] Warning: Could not read sensor state");
                        sensor_state.read().unwrap()
                    });

                    // Read device_label fresh from config for hot-reload support
                    let current_device_label = crate::libs::config::Config::load_default()
                        .ok()
                        .and_then(|cfg| cfg.system.device_label)
                        .unwrap_or_else(|| hostname.clone());

                    // Read LoRaWAN sensor state (sorted by dev_eui for consistent ordering)
                    let lorawan_sensors: Vec<LoRaWANSensorState> = if let Ok(ds) = display_state.lock() {
                        ds.lorawan_state.as_ref()
                            .and_then(|s| s.read().ok())
                            .map(|s| {
                                let mut sensors: Vec<LoRaWANSensorState> = s.sensors.values().cloned().collect();
                                sensors.sort_by(|a, b| a.dev_eui.cmp(&b.dev_eui));
                                sensors
                            })
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };

                    // Read button silence state for mute icon
                    let sensor_silenced = if let Ok(ds) = display_state.lock() {
                        ds.buzzer_priority.as_ref()
                            .map(|bp| bp.is_button_silenced())
                            .unwrap_or(false)
                    } else {
                        false
                    };

                    // Build the active-first ordered entries list for rendering
                    let entries = crate::libs::display::screens::ordered_sensors(
                        &sensor_snapshot.readings,
                        &lorawan_sensors,
                    );

                    // Render the sensor overview screen with network status and selection cursor
                    if let Err(e) = render_sensor_overview(&mut display, page, &led_snapshot, &sensor_snapshot, &network_status, selected_sensor, &current_device_label, lorawan_gateway_present, &lorawan_sensors, &entries, total_pages, sensor_silenced) {
                        eprintln!("[DisplayMonitor] Error rendering display: {}", e);
                    }
                }
                Screen::SensorDetail { sensor_idx } => {
                    // Read sensor state for temperature readings and thresholds
                    let sensor_snapshot = sensor_state.read().unwrap_or_else(|_| {
                        eprintln!("[DisplayMonitor] Warning: Could not read sensor state");
                        sensor_state.read().unwrap()
                    });

                    // Render the sensor detail screen with thresholds
                    if let Err(e) = render_sensor_detail(&mut display, sensor_idx, &sensor_snapshot) {
                        eprintln!("[DisplayMonitor] Error rendering sensor detail display: {}", e);
                    }
                }
                Screen::LoRaWANSensorDetail { dev_eui } => {
                    let (lorawan_sensor, detail_page, config_snapshot) = if let Ok(ds) = display_state.lock() {
                        let sensor = ds.lorawan_state.as_ref()
                            .and_then(|s| s.read().ok())
                            .and_then(|s| s.sensors.get(&dev_eui).cloned());
                        let page = ds.lorawan_detail_page;
                        let cfg = ds.lorawan_configs.as_ref()
                            .and_then(|c| c.read().ok())
                            .and_then(|v| v.iter().find(|c| c.dev_eui == dev_eui).cloned());
                        (sensor, page, cfg)
                    } else {
                        (None, 0, None)
                    };

                    if let Some(sensor) = lorawan_sensor {
                        if let Err(e) = render_lorawan_sensor_detail(
                            &mut display,
                            &sensor,
                            detail_page,
                            config_snapshot.as_ref(),
                        ) {
                            eprintln!("[DisplayMonitor] Error rendering LoRaWAN detail display: {}", e);
                        }
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

                    // Read device_label fresh from config for hot-reload support
                    let current_device_label = crate::libs::config::Config::load_default()
                        .ok()
                        .and_then(|cfg| cfg.system.device_label)
                        .unwrap_or_else(|| hostname.clone());

                    // Render system info screen with page number
                    if let Err(e) = render_system_info(
                        &mut display,
                        page,
                        &sensor_snapshot,
                        &network_status,
                        &power_snapshot,
                        &hostname,
                        &current_device_label,
                        &app_version,
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
                Screen::BleConnected { addr } => {
                    if let Err(e) = render_ble_connected(&mut display, &addr) {
                        eprintln!("[DisplayMonitor] Error rendering BLE connected: {}", e);
                    }
                }
                Screen::BleProvisioning { ssid } => {
                    if let Err(e) = render_ble_provisioning(&mut display, &ssid) {
                        eprintln!("[DisplayMonitor] Error rendering BLE provisioning: {}", e);
                    }
                }
                Screen::BleWifiOk { ssid, ip, .. } => {
                    if let Err(e) = render_ble_wifi_ok(&mut display, &ssid, &ip) {
                        eprintln!("[DisplayMonitor] Error rendering BLE wifi ok: {}", e);
                    }
                }
                Screen::BleWifiFail { error, .. } => {
                    if let Err(e) = render_ble_wifi_fail(&mut display, &error) {
                        eprintln!("[DisplayMonitor] Error rendering BLE wifi fail: {}", e);
                    }
                }
            }
        }

        // Sleep to prevent busy-waiting
        thread::sleep(Duration::from_millis(50));
    }

    eprintln!("[DisplayMonitor] Display monitor thread exited cleanly");
}
