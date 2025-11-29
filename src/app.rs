// src/app.rs
use crate::alarms::actions::HardwareAlarmActionSink;
use crate::audit::AuditSink;
use crate::audit_db::SqliteAuditSink;
use crate::blockchain::AuditBlockchain;
use crate::compaction::{CompactionScheduler, StorageMonitor};
use crate::drivers::stm::StmBridge;
use crate::drivers::ds2482::Ds2482Driver;
use crate::hal::real::{GpioBuzzerHal, StmLedHal, StmSensorLedHal};
use crate::logging::SqliteLogSink;
use crate::runtime::Runtime;
use crate::storage::SqliteTimeSeriesStore;
use crate::system::{GenericSensorNode, SensorSystem};

use crate::acquisition::AcquisitionConfig;
use crate::alarms::engine::AlarmConfig;
use crate::buttons::{Button, ButtonEvent, Buttons};
use crate::config::{AppConfig, SensorKind};
use crate::model::SensorId;
use crate::network::{spawn_http_server_with_state, SharedSensorReadings, SensorReading};
use crate::ui::{
    DisplayUiSink, SharedOverviewPage, SharedTestSelection, SharedUiMode, SystemInfo,
    SharedSystemInfo, SharedSelectedSensorId, TestMenuItem, UiMode,
};
use crate::power::{PowerStatus, SharedPowerStatus};
use crate::sensors::ds18b20::Ds18b20Backend;

use anyhow::{Context, Result};
use chrono::{Duration as ChronoDuration, Datelike, Utc};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

/// Type alias for the concrete runtime we’ll run on CM4.
type Cm4Runtime = Runtime<
    SqliteTimeSeriesStore,
    SqliteLogSink,
    HardwareAlarmActionSink<GpioBuzzerHal, StmLedHal, StmSensorLedHal>,
    DisplayUiSink,
>;

/// Build the full runtime with:
/// - SQLite time-series store (WAL)
/// - SQLite log sink
/// - STM-based LED HAL
/// - GPIO-based buzzer HAL
fn build_cm4_runtime(
    mode: SharedUiMode,
    test_selection: SharedTestSelection,
    overview_page: SharedOverviewPage,
    system_info: SharedSystemInfo,
    selected_sensor_id: SharedSelectedSensorId,
    power_status: SharedPowerStatus,
    stm: Rc<RefCell<StmBridge>>,
    discovery_rom_arcs: &mut HashMap<u8, Arc<Mutex<Option<String>>>>,
) -> Result<Cm4Runtime> {
    // 1) Time-series store (hot tier) with 7-day retention
    let retention = ChronoDuration::days(7);
    let store = SqliteTimeSeriesStore::open_file("fiber_readings.db", retention)
        .context("opening SQLite time-series store")?;

    // 2) Log sink
    let logger =
        SqliteLogSink::open_file("fiber_logs.db").context("opening SQLite log sink")?;

    // 4) Real buzzer + LED HALs
    let buzzer_hal = GpioBuzzerHal::new().context("initializing GPIO buzzer")?;
    let power_led_hal = StmLedHal::new(stm.clone());
    let sensor_led_hal = StmSensorLedHal::new(stm.clone());

    // 5) Load configuration (fiber.yaml or default demo)
    let cfg =
        AppConfig::load_from("fiber.yaml").context("loading fiber.yaml or default config")?;

    // 5.5) Initialize DS2482 driver (if needed)
    // Only initialize if any sensor has a fixed ROM code (not in discovery mode)
    let ds2482_driver: Option<Arc<Mutex<Ds2482Driver>>> = {
        let has_fixed_rom = cfg.sensors.iter()
            .any(|sc| matches!(sc.kind, SensorKind::Ds18b20) && sc.rom.is_some());

        if has_fixed_rom {
            // Find I2C path from config (use first one found, or default)
            let i2c_path = cfg.sensors.iter()
                .find_map(|sc| sc.i2c_path.as_ref())
                .map(|s| s.as_str())
                .unwrap_or("/dev/i2c-1");

            match Ds2482Driver::new(i2c_path) {
                Ok(driver) => {
                    println!("[fiber] DS2482 driver initialized on {}", i2c_path);
                    Some(Arc::new(Mutex::new(driver)))
                }
                Err(e) => {
                    eprintln!("[fiber] Warning: failed to initialize DS2482: {}", e);
                    eprintln!("[fiber] Discovery-mode sensors will still work via sysfs");
                    None
                }
            }
        } else {
            println!("[fiber] Using discovery-based sensors (sysfs mode) - DS2482 not required");
            None
        }
    };

    // 6) Build SensorSystem, SensorId -> LED index mapping and labels
    let mut system = SensorSystem::new();
    let mut sensor_led_map: HashMap<SensorId, u8> = HashMap::new();
    let mut label_map: HashMap<SensorId, String> = HashMap::new();

    // discovery_rom_arcs is passed in as a parameter and will be populated with ROM Arcs
    // (io_pin -> Arc<Mutex<Option<ROM>>>)

    for sc in cfg.sensors.iter() {
        let sid = SensorId(sc.id);

        // Label for the UI
        let label = sc
            .label
            .clone()
            .unwrap_or_else(|| format!("Sensor {}", sc.id));
        label_map.insert(sid, label);

        let acq = AcquisitionConfig::hundred_per_minute();

        let alarm = AlarmConfig {
            warning_low: sc.warning_low,
            warning_high: sc.warning_high,
            critical_low: sc.critical_low,
            critical_high: sc.critical_high,
            hysteresis: sc.hysteresis.unwrap_or(0.5),
        };

        match sc.kind {
            SensorKind::Simulated => {
                let base = sc.base_c.unwrap_or(36.5);
                let amp = sc.amplitude_c.unwrap_or(0.5);
                let period = sc.period_s.unwrap_or(60.0);
                // Use simulated backend with realistic temperature oscillation
                let backend = crate::sensors::simulated::SimulatedTemperatureBackend::new(sid, base, amp, period);
                let node = Box::new(GenericSensorNode::new(backend, acq, alarm));
                system.add_node(node);
                sensor_led_map.insert(sid, sc.led_index);
            }
            SensorKind::Ds18b20 => {
                // DS18B20 via DS2482S-800+ I2C bridge
                let io_pin = sc.io_pin
                    .ok_or_else(|| anyhow::anyhow!("ds18b20 sensor {} requires 'io_pin' field (0-7)", sc.id))?;

                if io_pin > 7 {
                    anyhow::bail!("ds18b20 sensor {} io_pin must be 0-7, got {}", sc.id, io_pin);
                }

                let calibration = sc.calibration_offset.unwrap_or(0.0);

                // Use discovery-based backend if ROM is not provided
                // Otherwise use driver-based backend
                if sc.rom.is_none() {
                    // Discovery mode: ROM will be found by auto-discovery
                    // Get or create the ROM Arc for this io_pin
                    eprintln!("[DEBUG] Sensor {} (id: {}): io_pin={}, creating discovery backend",
                              sid.0, sc.id, io_pin);

                    let rom_arc = discovery_rom_arcs
                        .entry(io_pin)
                        .or_insert_with(|| {
                            eprintln!("[DEBUG] ✓ Inserted new ROM Arc for io_pin {}", io_pin);
                            Arc::new(Mutex::new(None))
                        })
                        .clone();

                    eprintln!("[DEBUG] Created DiscoverySensorBackend for io_pin {}", io_pin);

                    let backend = crate::sensors::discovery_backend::DiscoverySensorBackend::new_with_rom(
                        sid,
                        io_pin,
                        calibration,
                        rom_arc,
                    );
                    let node = Box::new(GenericSensorNode::new(backend, acq, alarm));
                    system.add_node(node);
                    sensor_led_map.insert(sid, sc.led_index);
                } else {
                    // Fixed ROM mode: use driver-based backend
                    let rom_str = sc.rom.as_ref().unwrap();
                    let rom = parse_rom_code(rom_str)
                        .context(format!("parsing ROM code for sensor {}", sc.id))?;

                    let driver = ds2482_driver.as_ref()
                        .ok_or_else(|| anyhow::anyhow!("ds18b20 sensor {} configured but DS2482 driver not initialized", sc.id))?
                        .clone();

                    let backend = Ds18b20Backend::new(
                        sid,
                        rom,
                        io_pin,
                        calibration,
                        driver,
                    );
                    let node = Box::new(GenericSensorNode::new(backend, acq, alarm));
                    system.add_node(node);
                    sensor_led_map.insert(sid, sc.led_index);
                }
            }
        }
    }

    // 7) Hardware alarm action sink (PWRLED + buzzer + line LEDs)
    let actions =
        HardwareAlarmActionSink::new(buzzer_hal, power_led_hal, sensor_led_hal, sensor_led_map);

    // 8) UI sink: real LCD display
    let labels = Arc::new(label_map);
    let ui = DisplayUiSink::new(
        mode,
        test_selection,
        overview_page,
        system_info,
        selected_sensor_id,
        power_status,
        labels,
    )
    .context("initializing ST7920 display")?;

    // Debug: Log final state of discovery_rom_arcs HashMap
    eprintln!("[DEBUG] ========== build_cm4_runtime COMPLETE ==========");
    eprintln!("[DEBUG] discovery_rom_arcs has {} entries", discovery_rom_arcs.len());
    for (pin, _arc) in discovery_rom_arcs.iter() {
        eprintln!("[DEBUG]   ✓ io_pin {}: ROM Arc exists", pin);
    }
    if discovery_rom_arcs.is_empty() {
        eprintln!("[DEBUG] WARNING: No ROM Arcs created! Check if any sensors are in discovery mode");
    }
    eprintln!("[DEBUG] ==============================================");

    Ok(Runtime::new(system, store, logger, actions, ui))
}

/// Main CM4 loop: build runtime and tick forever.
pub fn run_cm4() -> Result<()> {
    // Shared UI state: mode + which test item is selected + overview page
    let mode: SharedUiMode = Arc::new(Mutex::new(UiMode::Overview));
    let test_selection: SharedTestSelection =
        Arc::new(Mutex::new(TestMenuItem::Accelerometer));
    let overview_page: SharedOverviewPage = Arc::new(Mutex::new(0usize));
    let system_info: SharedSystemInfo = Arc::new(Mutex::new(SystemInfo::default()));
    let selected_sensor_id: SharedSelectedSensorId = Arc::new(Mutex::new(None));
    // Initialize power status: 3400mV = 100%
    let power_status: SharedPowerStatus = Arc::new(Mutex::new(PowerStatus::from_vbat(3400)));

    // Create STM bridge once
    let stm = Rc::new(RefCell::new(
        StmBridge::new().context("opening STM bridge on /dev/ttyAMA4")?,
    ));

    // Activate sensor power pins at startup
    if let Err(e) = stm.borrow_mut().init_sensor_power() {
        eprintln!("[fiber] Warning: failed to initialize sensor power: {}", e);
    }

    // Initialize all LEDs to OFF state to ensure clean startup
    if let Err(e) = stm.borrow_mut().init_leds_off() {
        eprintln!("[fiber] Warning: failed to initialize LEDs: {}", e);
    }

    // Load HMAC key for audit system
    let hmac_key_data = std::fs::read("keys/hmac.key").unwrap_or_else(|_| {
        eprintln!("[audit] Warning: Could not load keys/hmac.key, using test key");
        [42u8; 32].to_vec()
    });
    let mut hmac_key = [0u8; 32];
    hmac_key.copy_from_slice(&hmac_key_data[..32.min(hmac_key_data.len())]);

    // Initialize audit sink
    let mut audit_sink = SqliteAuditSink::open_file("fiber_audit.db", hmac_key)
        .context("opening audit database")?;

    println!("[audit] Initialized. Entry count: {}", audit_sink.entry_count().unwrap_or(0));

    // Load blockchain key for Phase 2 immutable ledger
    let blockchain_key_data = std::fs::read("keys/blockchain.key").unwrap_or_else(|_| {
        eprintln!("[blockchain] Warning: Could not load keys/blockchain.key, using test key");
        [43u8; 32].to_vec()
    });
    let mut blockchain_key = [0u8; 32];
    blockchain_key.copy_from_slice(&blockchain_key_data[..32.min(blockchain_key_data.len())]);

    // Initialize blockchain for immutable monthly snapshots
    let mut blockchain = AuditBlockchain::new(blockchain_key);
    println!("[blockchain] Initialized. Height: {}", blockchain.height());

    // Create shared sensor readings state for HTTP API
    let sensor_readings: SharedSensorReadings = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    // Create shared slot status state for HTTP API with 8 empty slots
    let slots_state = std::sync::Arc::new(std::sync::Mutex::new(
        (0..8).map(|i| {
            crate::ui::dashboard::SlotStatus {
                slot_id: i,
                power_enabled: false,
                sensor_id: None,
                green_led: false,
                red_led: false,
                temperature: None,
                alarm_state: "Normal".to_string(),
            }
        }).collect::<Vec<_>>()
    ));

    // Start HTTP API in background thread with real slot state
    spawn_http_server_with_state("fiber.yaml", slots_state.clone(), sensor_readings.clone());

    println!("[fiber] runtime initialized. Entering main loop...");

    // Initialize storage compaction scheduler
    let mut compaction_scheduler = CompactionScheduler::new();
    let storage_monitor = StorageMonitor::new("/data/fiber".to_string());

    let mut enter_pressed_since: Option<std::time::Instant> = None;
    const LONG_PRESS_MS: u64 = 10_000;

    // Button sequence for on-demand 1-wire bus search: long press ENTER + fast click
    let mut enter_long_press_detected = false; // Set to true when ENTER long press is detected
    let mut waiting_for_click_after_long_press: Option<std::time::Instant> = None; // Timestamp when long press was released
    const CLICK_TIMEOUT_MS: u64 = 2000; // Max time window for fast click after long press (2 seconds)
    const FAST_CLICK_MAX_DURATION_MS: u64 = 500; // Max duration for a click to be considered "fast"

    // Power monitoring: update every 2 seconds
    let mut last_power_update = std::time::Instant::now();
    const POWER_UPDATE_INTERVAL_MS: u64 = 2000;
    let mut yellow_blink_state = false;
    let mut yellow_blink_counter = 0;
    const YELLOW_BLINK_PERIOD: u32 = 5; // Blink every 5 iterations (500ms)

    // Sensor discovery: scan every 3 seconds (balanced for responsiveness vs CPU efficiency)
    let mut last_discovery_time = std::time::Instant::now();
    const DISCOVERY_INTERVAL_MS: u64 = 3000;
    let mut discovered_sensors: Vec<crate::discovery::SensorOnChannel> = Vec::new();

    // Create this here so we can use it in both build_cm4_runtime and the main loop
    let mut discovery_rom_arcs: HashMap<u8, Arc<Mutex<Option<String>>>> = HashMap::new();

    // Track when sensors disconnect to implement grace period before clearing ROM Arc
    // Maps io_pin -> Instant of disconnection
    let mut disconnection_times: HashMap<u8, std::time::Instant> = HashMap::new();
    const GRACE_PERIOD_MS: u64 = 3000; // Keep ROM Arc for 3 seconds after sensor disappears

    // Track LED blink states for disconnected sensors
    // Maps io_pin -> last blink toggle time
    let mut led_blink_states: HashMap<u8, std::time::Instant> = HashMap::new();

    let mut runtime = build_cm4_runtime(
        mode.clone(),
        test_selection.clone(),
        overview_page.clone(),
        system_info.clone(),
        selected_sensor_id.clone(),
        power_status.clone(),
        stm.clone(),
        &mut discovery_rom_arcs,
    )?;

    // Spawn button polling thread for responsive button handling
    // This runs on a dedicated 1ms polling loop instead of the 100ms main loop
    eprintln!("[button] Spawning button polling thread...");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        match Buttons::new() {
            Ok(mut button_poller) => {
                eprintln!("[button] ✓ Button thread started, polling every 1ms");
                let mut poll_cycle = 0u64;
                loop {
                    poll_cycle += 1;
                    let events = button_poller.poll();
                    let event_count = events.len();
                    if event_count > 0 {
                        eprintln!("[button-poll] Poll cycle {}: {} event(s) detected", poll_cycle, event_count);
                    }
                    for event in events {
                        eprintln!("[button-poll] ✓ Event generated: {:?}", event);
                        match tx.send(event) {
                            Ok(_) => eprintln!("[button-send] ✓ Event sent to channel"),
                            Err(e) => eprintln!("[button-send] ✗ FAILED to send: {}", e),
                        }
                    }
                    thread::sleep(Duration::from_millis(1)); // 1ms polling loop
                }
            }
            Err(e) => {
                eprintln!("[button] ✗ Failed to initialize button poller thread: {}", e);
            }
        }
    });

    let mut button_events_received = 0u64;
    let mut main_loop_iterations = 0u64;
    loop {
        main_loop_iterations += 1;
        // --- BUTTONS / LONG PRESS + TEST MENU NAVIGATION ---
        // Non-blocking receive from button channel (processed every 100ms)
        while let Ok(event) = rx.try_recv() {
            button_events_received += 1;
            eprintln!("[button-recv] ✓ Event #{} received in main loop: {:?}", button_events_received, event);
            match event {
                ButtonEvent::Press(Button::Enter) => {
                    enter_pressed_since = Some(std::time::Instant::now());
                    enter_long_press_detected = false;
                }
                ButtonEvent::Release(Button::Enter) => {
                    let press_duration_ms = enter_pressed_since
                        .map(|t| t.elapsed().as_millis() as u64)
                        .unwrap_or(0);

                    // Check if this was a long press (>= LONG_PRESS_MS)
                    if press_duration_ms >= LONG_PRESS_MS {
                        // Long press detected - start waiting for a fast click
                        enter_long_press_detected = true;
                        waiting_for_click_after_long_press = Some(std::time::Instant::now());
                        eprintln!("[button] ENTER long press detected ({} ms), waiting for fast click...", press_duration_ms);
                    } else if let Some(long_press_release_time) = waiting_for_click_after_long_press {
                        // We already detected a long press and are waiting for a click
                        let time_since_long_press_release = long_press_release_time.elapsed().as_millis() as u64;

                        // Check if the fast click happened within the timeout window
                        if time_since_long_press_release <= CLICK_TIMEOUT_MS && press_duration_ms <= FAST_CLICK_MAX_DURATION_MS {
                            // Button sequence matched: long press + fast click
                            eprintln!("[button] ✓ Button sequence detected! Triggering 1-wire bus search on demand...");
                            eprintln!("[discovery] Manual trigger via button sequence (long press + fast click)");
                            let _ = crate::discovery::trigger_w1_search();

                            // Reset state
                            enter_long_press_detected = false;
                            waiting_for_click_after_long_press = None;
                        } else if time_since_long_press_release > CLICK_TIMEOUT_MS {
                            // Timeout exceeded, reset to idle state
                            waiting_for_click_after_long_press = None;
                        }
                        // Otherwise, this was just a normal click, do normal navigation (below)
                    }

                    // Short press ENTER: Navigation for different screens (only if no long press sequence in progress)
                    if !enter_long_press_detected && waiting_for_click_after_long_press.is_none() {
                        let current_mode = *mode.lock().unwrap();
                        match current_mode {
                            UiMode::Overview => {
                                // short ENTER on overview: nothing for now
                            }
                            UiMode::ServiceMenu => {
                                // Navigate to System Info (first menu item)
                                let mut m = mode.lock().unwrap();
                                *m = UiMode::SystemInfo;
                            }
                            UiMode::SystemInfo => {
                                // Go back to Service Menu
                                let mut m = mode.lock().unwrap();
                                *m = UiMode::ServiceMenu;
                            }
                            UiMode::SensorDetails => {
                                // Go back to Service Menu
                                let mut m = mode.lock().unwrap();
                                *m = UiMode::ServiceMenu;
                            }
                            UiMode::TestMenu => {
                                let sel = *test_selection.lock().unwrap();
                                let mut m = mode.lock().unwrap();
                                *m = match sel {
                                    TestMenuItem::Accelerometer => UiMode::TestAccel,
                                    TestMenuItem::Clock => UiMode::TestClock,
                                    TestMenuItem::LedTest => UiMode::TestLed,
                                };
                            }
                            UiMode::TestAccel | UiMode::TestClock | UiMode::TestLed => {
                                let mut m = mode.lock().unwrap();
                                *m = UiMode::TestMenu;
                            }
                        }
                    }

                    enter_pressed_since = None;
                }
                ButtonEvent::Press(Button::Up) => {
                    let current_mode = *mode.lock().unwrap();
                    match current_mode {
                        UiMode::Overview => {
                            let mut p = overview_page.lock().unwrap();
                            if *p > 0 {
                                *p -= 1;
                            }
                        }
                        UiMode::ServiceMenu => {
                            // Navigate in service menu (Up = go to previous item)
                            // For now just navigate to SystemInfo
                        }
                        UiMode::SensorDetails => {
                            // Navigate to previous sensor
                            let sel = selected_sensor_id.lock().unwrap();
                            if let Some(_current) = *sel {
                                // Find previous sensor
                                // For simplicity, we'll update this in a later version
                            }
                        }
                        UiMode::TestMenu => {
                            let mut sel = test_selection.lock().unwrap();
                            *sel = match *sel {
                                TestMenuItem::Accelerometer => TestMenuItem::LedTest,
                                TestMenuItem::Clock => TestMenuItem::Accelerometer,
                                TestMenuItem::LedTest => TestMenuItem::Clock,
                            };
                        }
                        _ => {}
                    }
                }
                ButtonEvent::Press(Button::Down) => {
                    let current_mode = *mode.lock().unwrap();
                    match current_mode {
                        UiMode::Overview => {
                            let mut p = overview_page.lock().unwrap();
                            *p += 1;
                        }
                        UiMode::ServiceMenu => {
                            // Navigate to Sensor Details
                            let mut m = mode.lock().unwrap();
                            *m = UiMode::SensorDetails;
                        }
                        UiMode::SensorDetails => {
                            // Navigate to next sensor
                            let sel = selected_sensor_id.lock().unwrap();
                            if let Some(_current) = *sel {
                                // Find next sensor
                                // For simplicity, we'll update this in a later version
                            }
                        }
                        UiMode::TestMenu => {
                            let mut sel = test_selection.lock().unwrap();
                            *sel = match *sel {
                                TestMenuItem::Accelerometer => TestMenuItem::Clock,
                                TestMenuItem::Clock => TestMenuItem::LedTest,
                                TestMenuItem::LedTest => TestMenuItem::Accelerometer,
                            };
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        } // End while let Ok(event) = rx.try_recv()

        // Check for long-press ENTER (2s) to toggle Overview <-> ServiceMenu
        if let Some(t0) = enter_pressed_since {
            if (t0.elapsed().as_millis() as u64) >= 2000 {
                {
                    let mut m = mode.lock().unwrap();
                    *m = match *m {
                        UiMode::Overview => UiMode::ServiceMenu,
                        UiMode::ServiceMenu => UiMode::Overview,
                        _ => UiMode::Overview,
                    };
                }
                println!("[fiber] toggled UI mode via long ENTER press");
                // Avoid multiple toggles without releasing
                enter_pressed_since = None;
            }
        }

        // Timeout handler for button sequence (long press + fast click)
        if let Some(release_time) = waiting_for_click_after_long_press {
            if (release_time.elapsed().as_millis() as u64) > CLICK_TIMEOUT_MS {
                eprintln!("[button] Button sequence timeout - no fast click detected within {} ms", CLICK_TIMEOUT_MS);
                waiting_for_click_after_long_press = None;
                enter_long_press_detected = false;
            }
        }

        // --- POWER MONITORING (every 2 seconds) ---
        if last_power_update.elapsed().as_millis() as u64 >= POWER_UPDATE_INTERVAL_MS {
            last_power_update = std::time::Instant::now();

            // Try to read VIN and VBAT from STM (only if not already borrowed)
            if let Ok(mut stm_ref) = stm.try_borrow_mut() {
                if let Ok((vin_opt, vbat_opt)) = stm_ref.read_adc_data() {
                    let vin_mv = vin_opt.map(|r| r.voltage_mv as u16).unwrap_or(0);
                    let vbat_mv = vbat_opt.map(|r| r.voltage_mv as u16).unwrap_or(0);

                    // Update power status
                    let new_power = PowerStatus::new(vbat_mv, vin_mv);
                    {
                        let mut ps = power_status.lock().unwrap();
                        *ps = new_power;
                    }

                    println!("[power] VIN={}mV VBAT={}mV ({}%) on_ac={}",
                        vin_mv, vbat_mv, new_power.battery_percent, new_power.on_ac_power);

                    // Update power LEDs based on actual power status
                    let (green_on, yellow_on) = new_power.get_pwr_led_state();
                    let _ = stm_ref.set_pwr_leds(green_on, yellow_on);
                }
            }
        }

        // --- YELLOW LED BLINKING (for low battery) ---
        yellow_blink_counter += 1;
        if yellow_blink_counter >= YELLOW_BLINK_PERIOD {
            yellow_blink_counter = 0;
            let ps = power_status.lock().unwrap();
            if ps.should_yellow_blink() {
                yellow_blink_state = !yellow_blink_state;
                // Only blink if not already borrowed
                if let Ok(mut stm_ref) = stm.try_borrow_mut() {
                    let _ = stm_ref.set_pwr_led_yellow(yellow_blink_state);
                }
            }
        }

        // --- SENSOR AUTO-DISCOVERY (every 5 seconds) ---
        if last_discovery_time.elapsed().as_millis() as u64 >= DISCOVERY_INTERVAL_MS {
            last_discovery_time = std::time::Instant::now();

            // NOTE: 1-wire bus search is now on-demand via button press (long press ENTER + fast click)
            // Previously called here every 3 seconds, which was blocking sensor reads
            // let _ = crate::discovery::trigger_w1_search();

            // Scan all channels for connected sensors
            match crate::discovery::scan_all_channels() {
                Ok(current_sensors) => {
                    // Debug: Show HashMap state when discovery finds sensors
                    eprintln!("[DEBUG] After scan_all_channels: discovery_rom_arcs has {} entries", discovery_rom_arcs.len());
                    for (pin, _arc) in discovery_rom_arcs.iter() {
                        eprintln!("[DEBUG]   ✓ io_pin {}: exists in HashMap", pin);
                    }
                    // Build a set of discovered ROMs for comparison
                    let current_roms: std::collections::HashSet<String> =
                        current_sensors.iter().map(|s| s.rom.clone()).collect();
                    let previous_roms: std::collections::HashSet<String> =
                        discovered_sensors.iter().map(|s| s.rom.clone()).collect();

                    // Update discovered sensors and feed ROMs to backends
                    for sensor in &current_sensors {
                        // Update ROM Arc for this io_pin (for discovery-based backends)
                        if let Some(rom_arc) = discovery_rom_arcs.get(&sensor.io_pin) {
                            if let Ok(mut rom) = rom_arc.lock() {
                                *rom = Some(sensor.rom.clone());
                                eprintln!("[discovery-diag] Updated ROM Arc for io_pin {} with ROM: {}", sensor.io_pin, sensor.rom);
                            }
                        } else {
                            eprintln!("[discovery-diag] ⚠ No ROM Arc found for io_pin {} (sensor: {})", sensor.io_pin, sensor.rom);
                        }

                        if !previous_roms.contains(&sensor.rom) {
                            println!(
                                "[discovery] ✓ Sensor detected on io_pin {}: {} ({})",
                                sensor.io_pin, sensor.rom, sensor.temperature_c
                            );

                            // Turn on LED green for this sensor
                            if let Ok(mut stm_ref) = stm.try_borrow_mut() {
                                // io_pin 0-7 maps to led_index 0-7
                                let led_idx = sensor.io_pin as u8;
                                if let Err(e) = stm_ref.set_line_leds(led_idx, true, false) {
                                    eprintln!("[discovery] Failed to set LED for io_pin {}: {}", sensor.io_pin, e);
                                }
                            }
                        }
                    }

                    // Detect newly disconnected sensors
                    for rom in previous_roms.iter() {
                        if !current_roms.contains(rom) {
                            println!("[discovery] ✗ Sensor disconnected: {}", rom);

                            // Find which io_pin this sensor was on
                            if let Some(sensor) = discovered_sensors.iter().find(|s| &s.rom == rom) {
                                // Record disconnection time (grace period before clearing ROM Arc)
                                disconnection_times.insert(sensor.io_pin, std::time::Instant::now());

                                // Track LED blink state for this io_pin during grace period
                                led_blink_states.insert(sensor.io_pin, std::time::Instant::now());

                                // Turn off LED for this sensor immediately
                                if let Ok(mut stm_ref) = stm.try_borrow_mut() {
                                    let led_idx = sensor.io_pin as u8;
                                    if let Err(e) = stm_ref.set_line_leds(led_idx, false, false) {
                                        eprintln!(
                                            "[discovery] Failed to turn off LED for io_pin {}: {}",
                                            sensor.io_pin, e
                                        );
                                    }
                                }
                            }
                        }
                    }

                    // Check if grace period has expired for any disconnected sensors
                    let now = std::time::Instant::now();
                    let mut io_pins_to_clear = Vec::new();

                    for (&io_pin, &disconnect_time) in disconnection_times.iter() {
                        let elapsed_ms = now.duration_since(disconnect_time).as_millis() as u64;
                        if elapsed_ms >= GRACE_PERIOD_MS {
                            io_pins_to_clear.push(io_pin);
                        } else {
                            // LED blinking during grace period: blink red every 500ms (250ms on, 250ms off)
                            if let Some(blink_time) = led_blink_states.get(&io_pin) {
                                let blink_elapsed = now.duration_since(*blink_time).as_millis() as u64;
                                let blink_cycle = blink_elapsed % 500;

                                // Turn red LED on for first 250ms, off for next 250ms
                                let red_on = blink_cycle < 250;

                                if let Ok(mut stm_ref) = stm.try_borrow_mut() {
                                    let led_idx = io_pin as u8;
                                    if let Err(e) = stm_ref.set_line_leds(led_idx, false, red_on) {
                                        eprintln!("[discovery] Failed to set blinking LED for io_pin {}: {}", io_pin, e);
                                    }
                                }
                            }
                            eprintln!("[discovery-diag] Grace period for io_pin {}: {}/{}ms", io_pin, elapsed_ms, GRACE_PERIOD_MS);
                        }
                    }

                    // Clear ROM Arc for sensors that have passed grace period
                    for io_pin in io_pins_to_clear {
                        if let Some(rom_arc) = discovery_rom_arcs.get(&io_pin) {
                            if let Ok(mut rom) = rom_arc.lock() {
                                *rom = None;
                                eprintln!("[discovery-diag] ✓ Grace period expired for io_pin {}, ROM Arc cleared", io_pin);
                                eprintln!("[discovery] Grace period expired for io_pin {}, ROM Arc cleared", io_pin);
                            }
                        }
                        disconnection_times.remove(&io_pin);
                        led_blink_states.remove(&io_pin);

                        // Make sure LED is off after grace period
                        if let Ok(mut stm_ref) = stm.try_borrow_mut() {
                            let led_idx = io_pin as u8;
                            if let Err(e) = stm_ref.set_line_leds(led_idx, false, false) {
                                eprintln!("[discovery] Failed to turn off LED for io_pin {}: {}", io_pin, e);
                            }
                        }
                    }

                    // Handle sensor reconnection: stop buzzer and reset LED to green
                    for sensor in &current_sensors {
                        if disconnection_times.contains_key(&sensor.io_pin) {
                            // Sensor reconnected during grace period
                            eprintln!("[discovery] ✓ Sensor reconnected on io_pin {}: {}", sensor.io_pin, sensor.rom);

                            // Remove from disconnection tracking
                            disconnection_times.remove(&sensor.io_pin);
                            led_blink_states.remove(&sensor.io_pin);

                            // Turn LED green for reconnected sensor
                            if let Ok(mut stm_ref) = stm.try_borrow_mut() {
                                let led_idx = sensor.io_pin as u8;
                                if let Err(e) = stm_ref.set_line_leds(led_idx, true, false) {
                                    eprintln!("[discovery] Failed to set LED green for io_pin {}: {}", sensor.io_pin, e);
                                }
                            }
                        }
                    }

                    // Update discovered sensors state
                    discovered_sensors = current_sensors;
                }
                Err(e) => {
                    eprintln!("[discovery] Error scanning channels: {}", e);
                }
            }
        }

        // --- PROCESS PENDING BUTTON EVENTS BEFORE BLOCKING SENSOR READS ---
        // This ensures buttons are responsive even during sensor acquisition
        while let Ok(event) = rx.try_recv() {
            button_events_received += 1;
            eprintln!("[button-recv-emergency] ✓ Event #{} received during sensor read: {:?}", button_events_received, event);
            match event {
                ButtonEvent::Press(Button::Down) => {
                    let current_mode = *mode.lock().unwrap();
                    if matches!(current_mode, UiMode::Overview) {
                        let mut p = overview_page.lock().unwrap();
                        *p += 1;
                        eprintln!("[button-action] Overview page changed to page {} during sensor read", *p);
                    }
                }
                ButtonEvent::Press(Button::Up) => {
                    let current_mode = *mode.lock().unwrap();
                    if matches!(current_mode, UiMode::Overview) {
                        let mut p = overview_page.lock().unwrap();
                        if *p > 0 {
                            *p -= 1;
                        }
                        eprintln!("[button-action] Overview page changed to page {} during sensor read", *p);
                    }
                }
                _ => {} // Other events handled in main loop
            }
        }

        // --- RUNTIME TICK ---
        let now = Utc::now();
        let tick_result = runtime.tick(now);

        // Update shared sensor readings for HTTP API based on alarm events
        if let Ok(mut readings) = sensor_readings.lock() {
            // Update alarm states from alarm events
            for event in &tick_result.alarm_events {
                let alarm_state = format!("{:?}", event.to);
                readings.entry(event.sensor_id.0)
                    .and_modify(|r| r.alarm_state = alarm_state.clone())
                    .or_insert_with(|| SensorReading {
                        sensor_id: event.sensor_id.0,
                        slot_id: (event.sensor_id.0 - 1) as usize,
                        temperature: Some(event.value),
                        alarm_state,
                        power_enabled: true,
                        connected: true,
                    });
            }
        }

        // Mirror sensor readings to slots state for HTTP API
        if let Ok(mut slots) = slots_state.lock() {
            if let Ok(readings) = sensor_readings.lock() {
                for (sensor_id, reading) in readings.iter() {
                    if reading.slot_id < 8 {
                        slots[reading.slot_id].temperature = reading.temperature;
                        slots[reading.slot_id].alarm_state = reading.alarm_state.clone();
                        slots[reading.slot_id].sensor_id = Some(*sensor_id as u32);
                        slots[reading.slot_id].power_enabled = reading.power_enabled;
                    }
                }
            }
        }

        // Record alarm events to audit log and blockchain
        for ev in &tick_result.alarm_events {
            let entry = crate::audit::AuditEntry::from(ev);
            if let Err(e) = audit_sink.record_event(entry.clone()) {
                eprintln!("[audit] Failed to record alarm: {}", e);
            }
            // Add to pending blockchain entries
            blockchain.add_pending_entry(entry);
        }

        // Check if blockchain should mine a new block
        if blockchain.should_mine_block(now) {
            println!("[blockchain] Mining block #{} from {} pending entries...", blockchain.height(), blockchain.pending_entries().len());
            match blockchain.mine_block(now) {
                Ok(block) => {
                    println!("[blockchain] ✓ Mined block #{}: {} entries, hash={}",
                        block.index, block.entries_count, &block.hash[..16]);
                }
                Err(e) => {
                    eprintln!("[blockchain] Failed to mine block: {}", e);
                }
            }
        }

        // --- STORAGE COMPACTION ---

        // Weekly aggregation (every Monday midnight)
        if compaction_scheduler.should_aggregate_weekly(now) {
            match crate::compaction::aggregate_readings_to_minutes("fiber_readings.db", now, now) {
                Ok(deleted) => {
                    compaction_scheduler.mark_weekly_done(now);
                    println!("[compaction] ✓ Weekly aggregation completed, deleted {} entries",
                        deleted);
                }
                Err(e) => {
                    eprintln!("[compaction] Weekly aggregation failed: {}", e);
                }
            }
        }

        // Monthly archival (every 1st of month)
        if compaction_scheduler.should_archive_monthly(now) {
            // Archive previous month's readings
            let month_to_archive = if now.month() == 1 {
                (now.year() - 1, 12)
            } else {
                (now.year(), now.month() - 1)
            };

            match crate::compaction::archive_month_readings(
                "fiber_readings.db",
                "/data/fiber",
                month_to_archive.0 as i32,
                month_to_archive.1,
            ) {
                Ok(_) => {
                    compaction_scheduler.mark_monthly_done(now);
                    println!("[compaction] ✓ Monthly archival completed for {}-{:02}",
                        month_to_archive.0, month_to_archive.1);

                    // Cleanup old aggregates (keep 90 days)
                    if let Ok(cleanup_deleted) = crate::compaction::cleanup_old_aggregates(
                        "fiber_readings.db",
                        90,
                    ) {
                        println!("[compaction] ✓ Cleanup: deleted {} old aggregates", cleanup_deleted);
                    }
                }
                Err(e) => {
                    eprintln!("[compaction] Monthly archival failed: {}", e);
                }
            }
        }

        // Disk usage monitoring (every hour)
        if compaction_scheduler.should_check_disk(now) {
            match storage_monitor.check_disk_usage() {
                Ok(status) => {
                    compaction_scheduler.mark_disk_check_done(now);
                    if status.is_critical {
                        eprintln!("[CRITICAL] Disk {}% full - immediate compaction needed!",
                            status.used_percent as i32);
                    } else if status.is_warning {
                        println!("[WARNING] Disk {}% full - monitor space usage",
                            status.used_percent as i32);
                    }
                }
                Err(e) => {
                    eprintln!("[compaction] Disk check failed: {}", e);
                }
            }
        }

        // Diagnostic: Report periodically if no button events received
        if main_loop_iterations % 10 == 0 {
            if button_events_received == 0 {
                eprintln!("[button-recv] ⚠ No button events received yet after {} main loop iterations ({}ms)",
                    main_loop_iterations, main_loop_iterations * 100);
            } else {
                eprintln!("[button-recv] Status: {} total button events received in {} iterations",
                    button_events_received, main_loop_iterations);
            }
        }

        // Tick rate: 100 ms
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Parse a ROM code from hex string format
///
/// Input format: "28-00-00-00-ab-cd-ef-00" (8 bytes hex separated by dashes)
/// Output: [0x28, 0x00, 0x00, 0x00, 0xAB, 0xCD, 0xEF, 0x00]
fn parse_rom_code(s: &str) -> anyhow::Result<[u8; 8]> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 8 {
        anyhow::bail!(
            "ROM code must have 8 hex bytes separated by dashes, got {} parts",
            parts.len()
        );
    }

    let mut rom = [0u8; 8];
    for (i, part) in parts.iter().enumerate() {
        rom[i] = u8::from_str_radix(part, 16)
            .with_context(|| format!("parsing ROM byte {}: invalid hex '{}'", i, part))?;
    }
    Ok(rom)
}
