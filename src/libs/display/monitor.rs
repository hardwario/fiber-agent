//! Display monitor thread - continuously updates the ST7920 display

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rppal::gpio::Gpio;

use crate::drivers::display::St7920;
use crate::libs::alarms::color::LedColor;
use crate::libs::leds::SharedLedStateHandle;
use crate::libs::lorawan::LoRaWANSensorState;
use crate::libs::network::get_network_status;
use crate::libs::power::SharedPowerStatus;
use crate::libs::sensors::SharedSensorStateHandle;

use super::blank;
use super::screens::{
    render_ble_connected, render_ble_provisioning, render_ble_wifi_fail, render_ble_wifi_ok,
    render_custom_overview, render_lorawan_sensor_detail, render_pairing_screen,
    render_qr_code_screen, render_qr_session_ended_screen, render_sensor_detail,
    render_sensor_overview, render_system_info,
};
use super::supervise::{lock_recover, read_recover, write_recover};
use super::{Screen, SharedDisplayLinesHandle, SharedDisplayStateHandle};

/// How often the loop re-reads the config file to pick up out-of-band edits
/// (a hand-edited YAML, or a write from another process).
///
/// This used to happen on every frame — 4-8 full `Config::load_default()` calls
/// per second, each of which runs the migration check and would rewrite the file
/// on a version mismatch. Pushed changes don't wait for this: the MQTT executor
/// writes the shared handle directly, so the reconcile is only the slow path.
const CONFIG_RECONCILE_MS: u64 = 2000;

/// Snapshot of the config values the display loop cares about.
struct DisplayConfigSnapshot {
    device_label: String,
    custom_lines: Vec<crate::libs::config::DisplayLine>,
}

/// Re-read the display-relevant config from disk, or `None` if it can't be read.
///
/// `None` rather than a default-valued snapshot: the caller keeps the last good
/// values instead. A transient read or parse failure must not blank the device
/// label back to the hostname, and above all must not clear `custom_lines` —
/// that would revert the panel to the built-in layout *and* wipe the shared
/// handle the MQTT executor writes to.
fn load_config_snapshot(hostname: &str) -> Option<DisplayConfigSnapshot> {
    let cfg = crate::libs::config::Config::load_default().ok()?;
    Some(DisplayConfigSnapshot {
        device_label: cfg
            .system
            .device_label
            .unwrap_or_else(|| hostname.to_string()),
        custom_lines: cfg.display.custom_lines,
    })
}

/// Decide the backlight brightness to apply.
///
/// Returns the `configured` brightness when the display should be lit, or `0`
/// when it should be off. The display stays lit when an alarm forces it
/// (`force_lit`), when the idle timeout is disabled (`timeout` is zero), or
/// while the time since the last activity (`idle`) is still within `timeout`.
fn effective_backlight(configured: u8, idle: Duration, timeout: Duration, force_lit: bool) -> u8 {
    if force_lit || timeout.is_zero() || idle < timeout {
        configured
    } else {
        0
    }
}

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
    screen_timeout: Arc<AtomicU32>,
    display_lines: SharedDisplayLinesHandle,
) -> Result<(), String> {
    // Initialize display. Reported as an error rather than a quiet return so
    // the supervisor retries — re-running init() is the recovery path, and a
    // controller that isn't ready yet at boot is exactly what it's for.
    let mut display =
        St7920::new(gpio).map_err(|e| format!("failed to initialize display: {}", e))?;
    eprintln!("[DisplayMonitor] Display initialized successfully");

    // Boot splash: render the HARDWARIO logo once and dwell for a short
    // moment before the normal render loop takes over. Doing it here
    // (inside the display thread, after St7920::new) keeps main.rs out of
    // the drawing path — the display is only owned by this thread.
    // Skipped when a power-off is already pending: this loop restarting at just
    // the wrong moment must not answer a shutdown with a two-second logo dwell.
    // Falling through leaves the panel untouched for one 50 ms iteration, then
    // the blanking arm below clears it.
    if blank::blank_state() == blank::LIVE {
        display.clear_buffer();
        super::splash::render_splash(&mut display);
        if let Err(e) = display.flush() {
            eprintln!("[DisplayMonitor] Boot splash flush failed: {}", e);
        }
        thread::sleep(super::splash::SPLASH_DURATION);
    }

    const UPDATE_INTERVAL_MS: u64 = 250; // Update display every 250ms
    let update_interval = Duration::from_millis(UPDATE_INTERVAL_MS);
    let mut last_update = std::time::Instant::now();

    // Track last applied brightness to detect changes
    let mut last_brightness: u8 = 100; // Default to full brightness

    // Config values re-read periodically rather than per frame (see
    // CONFIG_RECONCILE_MS). Seeded and published once here, before the first
    // frame, so the in-loop reconcile can be a pure change-detector: the
    // snapshot it compares against and the handle it writes start in agreement.
    let reconcile_interval = Duration::from_millis(CONFIG_RECONCILE_MS);
    // Whether the last reconcile could read the file, so the failure is logged
    // on the transition rather than every 2 s forever.
    let mut config_readable = true;
    let mut config_snapshot = match load_config_snapshot(&hostname) {
        Some(snapshot) => {
            *write_recover(&display_lines) = snapshot.custom_lines.clone();
            snapshot
        }
        None => {
            // Unreadable at startup: keep whatever main.rs seeded the handle
            // with, which is the same file read a moment earlier.
            eprintln!(
                "[DisplayMonitor] Config unreadable at startup, using the seeded display config"
            );
            config_readable = false;
            DisplayConfigSnapshot {
                device_label: hostname.clone(),
                custom_lines: read_recover(&display_lines).clone(),
            }
        }
    };
    let mut last_config_reconcile = std::time::Instant::now();

    eprintln!(
        "[DisplayMonitor] Started display loop with {}ms update interval",
        UPDATE_INTERVAL_MS
    );

    // Main display loop
    loop {
        // Check for shutdown signal
        if shutdown_flag.load(Ordering::Relaxed) {
            eprintln!("[DisplayMonitor] Shutdown signal received, exiting display thread");
            break;
        }

        // Power-off blanking, checked before the backlight and render blocks so
        // neither can repaint the panel we just cleared. Deliberately not a
        // `return Ok(())`: `supervise` treats a clean return as "done" and never
        // restarts the loop, which would leave a permanently dead UI on a
        // power-off that failed to happen.
        match blank::blank_state() {
            blank::REQUESTED => {
                display.clear_buffer();
                if let Err(e) = display.flush() {
                    eprintln!("[DisplayMonitor] Power-off blank flush failed: {}", e);
                }
                if let Err(e) = display.set_brightness(0) {
                    eprintln!("[DisplayMonitor] Power-off backlight off failed: {}", e);
                }
                // The brightness block below only writes the PWM on a change, so
                // this has to reflect what the hardware is actually at — leaving
                // it stale would strand the panel dark if the blank is cancelled.
                last_brightness = 0;
                eprintln!("[DisplayMonitor] Panel blanked for power-off");
                blank::mark_blanked();
                continue;
            }
            blank::BLANKED => {
                // Hold it: no reconcile, no render, just keep watching the
                // shutdown flag and the cancel above.
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            // Live — including the frame right after a cancel, which repaints
            // and restores the backlight through the normal path below.
            _ => {}
        }

        // Backlight idle timeout: off after `screen_timeout` of inactivity, but
        // a critical error (red LED: Critical / Disconnected / Reconnecting) or
        // audible alert forces it lit. Non-critical warnings (yellow) do not
        // keep the screen awake. The configured brightness is preserved and
        // restored on wake.
        let alarm_lit = led_state
            .read()
            .lines
            .iter()
            .flatten()
            .any(|l| l.led_state.color == LedColor::Red);
        let (idle, force_lit) = {
            let mut ds = lock_recover(&display_state);
            let force = alarm_lit
                || ds
                    .buzzer_priority
                    .as_ref()
                    .is_some_and(|bp| bp.is_sensor_beeping());
            // Keep the timer fresh while lit so a full timeout starts once the
            // alarm clears (alarm onset counts as activity).
            if force {
                ds.mark_activity();
            }
            (ds.last_activity.elapsed(), force)
        };
        // Read the idle timeout live each tick so runtime changes (e.g. via
        // MQTT) take effect without a restart. 0 disables the timeout.
        let timeout = Duration::from_secs(u64::from(screen_timeout.load(Ordering::Relaxed)));
        let target_brightness = effective_backlight(
            screen_brightness.load(Ordering::Relaxed),
            idle,
            timeout,
            force_lit,
        );
        if target_brightness != last_brightness {
            // On idle timeout only the backlight is cut (PWM to 0); the panel
            // keeps being rendered below so its content stays current and simply
            // reappears when the backlight returns on the next activity.
            if let Err(e) = display.set_brightness(target_brightness) {
                eprintln!(
                    "[DisplayMonitor] Failed to set backlight to {}%: {}",
                    target_brightness, e
                );
            } else if target_brightness == 0 {
                eprintln!("[DisplayMonitor] Backlight off (idle timeout)");
            } else {
                eprintln!("[DisplayMonitor] Backlight set to {}%", target_brightness);
            }
            last_brightness = target_brightness;
        }

        // Periodically re-read the config file so out-of-band edits (hand-edited
        // YAML, or a write from fiberctl) are picked up. MQTT-pushed changes go
        // straight to the shared handle and don't wait for this.
        if last_config_reconcile.elapsed() >= reconcile_interval {
            last_config_reconcile = std::time::Instant::now();
            match load_config_snapshot(&hostname) {
                Some(fresh) => {
                    if !config_readable {
                        eprintln!("[DisplayMonitor] Config readable again, resuming reconcile");
                        config_readable = true;
                    }
                    // Only push on an actual on-disk change. An unconditional
                    // write would race the MQTT executor, which writes the file
                    // and then the handle: a reconcile that read the file just
                    // before that write would otherwise stamp the pre-push value
                    // back over the new one for a full interval.
                    if fresh.custom_lines != config_snapshot.custom_lines {
                        *write_recover(&display_lines) = fresh.custom_lines.clone();
                    }
                    config_snapshot = fresh;
                }
                None => {
                    // Keep the last good snapshot. Logged once per transition,
                    // not every 2 s.
                    if config_readable {
                        eprintln!(
                            "[DisplayMonitor] Config unreadable, keeping the last known display config"
                        );
                        config_readable = false;
                    }
                }
            }
        }

        // Throttle updates to reduce flicker and CPU usage
        if last_update.elapsed() >= update_interval {
            last_update = std::time::Instant::now();

            // Fetch current network status
            let network_status = get_network_status();

            // Snapshot the configured lines *before* taking the display_state
            // mutex — never hold that lock while acquiring another.
            let custom_lines = read_recover(&display_lines).clone();

            // Same reason, plus `overview_mode()` needs these to size the
            // default row set: sensors that have never reported are hidden, so
            // the row count is the filtered one. Cloned rather than held as a
            // guard so no sensor-state reader is blocked for the whole frame.
            let (ds_readings, ds_has_reported) = {
                let snapshot = read_recover(&sensor_state);
                (snapshot.readings.clone(), snapshot.has_reported)
            };

            // Get current display state (screen and page)
            let (
                current_screen,
                qr_generator,
                lorawan_gateway_present,
                overview_mode,
                hold_bar_pixels,
            ) = {
                let mut state = lock_recover(&display_state);
                // Revert any expired timed screens (BleWifiOk / BleWifiFail) before rendering
                state.tick_timed_screens();
                // Update network status in display state
                state.network_status = network_status.clone();
                // Publish the live line count so the button thread pages over
                // the same rows this frame is about to draw.
                state.custom_line_count = custom_lines.len();
                let mode = state.overview_mode(&ds_readings, &ds_has_reported);
                // Pull the active QR generator out of the live provisioning
                // session (if any). None ⇒ either prov mode not entered or
                // session ended → QR screen will fall through to a notice.
                let qr = state.provisioning_session.as_ref().and_then(|s| {
                    s.read()
                        .ok()
                        .and_then(|g| g.as_ref().map(|sess| sess.qr_generator()))
                });
                (
                    state.current_screen.clone(),
                    qr,
                    state.lorawan_gateway_present,
                    mode,
                    state.hold_bar_pixels,
                )
            };
            let total_pages = overview_mode.total_pages();

            // Read LED state to determine sensor status
            let led_snapshot = led_state.read();

            // Dispatch rendering based on current screen
            match current_screen {
                Screen::SensorOverview {
                    page,
                    selected_sensor,
                } => {
                    // Read sensor state for temperature readings
                    let sensor_snapshot = read_recover(&sensor_state);

                    let current_device_label = &config_snapshot.device_label;

                    // Clone the LoRa handle and silence flag out from under the
                    // display_state mutex in one pass, then drop it before
                    // taking the inner RwLock.
                    let (lorawan_state_arc, sensor_silenced) = {
                        let ds = lock_recover(&display_state);
                        let silenced = ds
                            .buzzer_priority
                            .as_ref()
                            .map(|bp| bp.is_button_silenced())
                            .unwrap_or(false);
                        (ds.lorawan_state.clone(), silenced)
                    };

                    // Read LoRaWAN sensor state (sorted by dev_eui for consistent ordering)
                    let lorawan_sensors: Vec<LoRaWANSensorState> = lorawan_state_arc
                        .as_ref()
                        .map(|s| {
                            let mut sensors: Vec<LoRaWANSensorState> =
                                read_recover(s).sensors.values().cloned().collect();
                            sensors.sort_by(|a, b| a.dev_eui.cmp(&b.dev_eui));
                            sensors
                        })
                        .unwrap_or_default();

                    // Custom lines only apply in page mode: selection mode falls
                    // back to the canonical sensor list so every physical sensor's
                    // detail screen stays reachable. `overview_mode` already
                    // encodes that decision — this is the same value the page
                    // count above was derived from, so the two cannot disagree.
                    let render_result = if overview_mode.is_custom() {
                        let rows = crate::libs::display::overview::build_custom_lines(
                            &custom_lines,
                            &sensor_snapshot.readings,
                            &sensor_snapshot.names,
                            &lorawan_sensors,
                        );
                        render_custom_overview(
                            &mut display,
                            page,
                            &network_status,
                            current_device_label,
                            lorawan_gateway_present,
                            &rows,
                            total_pages,
                            sensor_silenced,
                            hold_bar_pixels,
                        )
                    } else {
                        // Build the active-first ordered entries list for rendering.
                        // Sensors that never reported are filtered out here, so the
                        // page count comes from the surviving entries rather than
                        // `overview_mode`: identical arithmetic, but derived from the
                        // exact list being drawn, so a reading that lands between the
                        // two snapshots can't make the count disagree with the rows.
                        let entries = crate::libs::display::screens::ordered_sensors(
                            &sensor_snapshot.readings,
                            &sensor_snapshot.has_reported,
                            &lorawan_sensors,
                        );
                        let total_pages = crate::libs::display::screens::page_count(&entries);

                        // A sensor disappearing can leave the stored page or cursor
                        // out of range — reconcile first, then render what the state
                        // actually holds so the frame matches it.
                        let (page, selected_sensor) = {
                            let mut ds = lock_recover(&display_state);
                            ds.clamp_overview(&entries);
                            match ds.current_screen {
                                Screen::SensorOverview {
                                    page,
                                    selected_sensor,
                                } => (page, selected_sensor),
                                // Screen changed under us (button press between the
                                // snapshot and now) — draw the snapshot, the next
                                // frame picks up the new screen.
                                _ => (page.min(total_pages - 1), selected_sensor),
                            }
                        };

                        render_sensor_overview(
                            &mut display,
                            page,
                            &led_snapshot,
                            &sensor_snapshot,
                            &network_status,
                            selected_sensor,
                            current_device_label,
                            lorawan_gateway_present,
                            &lorawan_sensors,
                            &entries,
                            total_pages,
                            sensor_silenced,
                            hold_bar_pixels,
                        )
                    };
                    if let Err(e) = render_result {
                        eprintln!("[DisplayMonitor] Error rendering display: {}", e);
                    }
                }
                Screen::SensorDetail { sensor_idx } => {
                    // Read sensor state for temperature readings and thresholds
                    let sensor_snapshot = read_recover(&sensor_state);

                    // Render the sensor detail screen with thresholds
                    if let Err(e) = render_sensor_detail(&mut display, sensor_idx, &sensor_snapshot)
                    {
                        eprintln!(
                            "[DisplayMonitor] Error rendering sensor detail display: {}",
                            e
                        );
                    }
                }
                Screen::LoRaWANSensorDetail { dev_eui } => {
                    // Clone Arc handles + page index out of display_state under its Mutex,
                    // then drop the Mutex before taking the inner RwLocks. Avoids holding
                    // the Mutex while readers/writers contend on the LoRa handles.
                    let (lorawan_state_arc, lorawan_configs_arc, detail_page) =
                        if let Ok(ds) = display_state.lock() {
                            (
                                ds.lorawan_state.clone(),
                                ds.lorawan_configs.clone(),
                                ds.lorawan_detail_page,
                            )
                        } else {
                            (None, None, 0)
                        };

                    let lorawan_sensor = lorawan_state_arc
                        .as_ref()
                        .and_then(|s| s.read().ok())
                        .and_then(|s| s.sensors.get(&dev_eui).cloned());
                    let config_snapshot = lorawan_configs_arc
                        .as_ref()
                        .and_then(|c| c.read().ok())
                        .and_then(|v| v.iter().find(|c| c.dev_eui == dev_eui).cloned());

                    if let Some(sensor) = lorawan_sensor {
                        if let Err(e) = render_lorawan_sensor_detail(
                            &mut display,
                            &sensor,
                            detail_page,
                            config_snapshot.as_ref(),
                        ) {
                            eprintln!(
                                "[DisplayMonitor] Error rendering LoRaWAN detail display: {}",
                                e
                            );
                        }
                    }
                }
                Screen::QrCodeConfig => {
                    // Render QR code configuration screen. With ephemeral
                    // provisioning sessions, the QR is only available while
                    // a session is active; otherwise show a session-ended
                    // notice so the user knows the QR is no longer valid.
                    if let Some(qr_gen) = qr_generator {
                        if let Err(e) = render_qr_code_screen(&mut display, &led_snapshot, &qr_gen)
                        {
                            eprintln!("[DisplayMonitor] Error rendering QR code display: {}", e);
                        }
                    } else if let Err(e) = render_qr_session_ended_screen(&mut display) {
                        eprintln!(
                            "[DisplayMonitor] Error rendering session-ended screen: {}",
                            e
                        );
                    }
                }
                Screen::SystemInfo { page } => {
                    // Read sensor state for probe count
                    let sensor_snapshot = read_recover(&sensor_state);

                    // Read power status
                    let power_snapshot = if let Ok(ps) = power_status.lock() {
                        *ps
                    } else {
                        eprintln!("[DisplayMonitor] Warning: Could not read power status");
                        crate::libs::power::PowerStatus::default()
                    };

                    let current_device_label = &config_snapshot.device_label;

                    // Render system info screen with page number
                    if let Err(e) = render_system_info(
                        &mut display,
                        page,
                        &sensor_snapshot,
                        &network_status,
                        &power_snapshot,
                        &hostname,
                        current_device_label,
                        &app_version,
                    ) {
                        eprintln!(
                            "[DisplayMonitor] Error rendering system info display: {}",
                            e
                        );
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
    Ok(())
}
