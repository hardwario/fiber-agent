// Background sensor monitoring thread

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::drivers::StmBridge;
use crate::libs::alarms::{
    AlarmController, AlarmState, BuzzerCallback, LoggingCallback, MqttAlarmCallback,
};
use crate::libs::buzzer::{BuzzerController, BuzzerPattern, BuzzerPriorityManager};
use crate::libs::config::{Config, SensorConfig, SensorFileConfig};
use crate::libs::leds::SharedLedStateHandle;
use crate::libs::mqtt::MqttHandle;
use crate::libs::storage::models::MinuteAggregateRow;
use crate::libs::storage::StorageHandle;

use super::aggregation::{AggregationPeriod, AggregationState, SensorAggregation};
use super::reader::W1DeviceReader;
use super::state::{SensorReading, SharedSensorStateHandle};

const W1_BASE_PATH_DEFAULT: &str = "/sys/bus/w1/devices";
const SENSOR_CONFIG_FILE: &str = "/data/fiber/config/fiber.sensors.config.yaml";

/// Base path for the 1-Wire sysfs tree. `FIBER_W1_BASE_PATH` lets a test or a local
/// run point the monitor at a fabricated tree instead of real sysfs.
fn w1_base_path() -> String {
    std::env::var("FIBER_W1_BASE_PATH").unwrap_or_else(|_| W1_BASE_PATH_DEFAULT.to_string())
}

/// True at most once per `period_secs` for the given slot.
///
/// `slot` holds the unix-seconds stamp of the last time this returned true; a fresh
/// slot (0) always fires. Used to keep a per-cycle log line from becoming a flood.
fn throttle(slot: &AtomicU64, period_secs: u64) -> bool {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let last = slot.load(Ordering::Relaxed);
    if now_secs >= last + period_secs {
        slot.store(now_secs, Ordering::Relaxed);
        true
    } else {
        false
    }
}

/// Background sensor monitoring thread
pub struct SensorMonitor {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
    buzzer: Arc<Mutex<BuzzerController>>,
}

impl SensorMonitor {
    /// Create and spawn background sensor monitoring thread
    pub fn new(
        config: SensorConfig,
        stm: Arc<Mutex<StmBridge>>,
        led_state: SharedLedStateHandle,
        buzzer: Arc<Mutex<BuzzerController>>,
        sensor_state: SharedSensorStateHandle,
        priority_manager: Arc<BuzzerPriorityManager>,
        mqtt_handle: Option<MqttHandle>,
        storage_handle: Option<StorageHandle>,
    ) -> io::Result<Self> {
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();

        let buzzer_clone = buzzer.clone();

        // Spawn the main sensor monitoring thread
        let thread_handle = thread::spawn(move || {
            Self::monitor_loop(
                config,
                stm,
                shutdown_flag_clone,
                buzzer_clone,
                led_state,
                sensor_state,
                priority_manager,
                mqtt_handle,
                storage_handle,
            );
        });

        Ok(Self {
            thread_handle: Some(thread_handle),
            shutdown_flag,
            buzzer,
        })
    }

    /// Background monitoring loop
    fn monitor_loop(
        config: SensorConfig,
        stm: Arc<Mutex<StmBridge>>,
        shutdown_flag: Arc<AtomicBool>,
        buzzer: Arc<Mutex<BuzzerController>>,
        led_state: SharedLedStateHandle,
        sensor_state: SharedSensorStateHandle,
        priority_manager: Arc<BuzzerPriorityManager>,
        mqtt_handle: Option<MqttHandle>,
        storage_handle: Option<StorageHandle>,
    ) {
        // Load sensor file configuration
        let sensor_file_config = match SensorFileConfig::load_default() {
            Ok(cfg) => {
                eprintln!(
                    "[SensorMonitor] Loaded sensor configuration from {}",
                    SENSOR_CONFIG_FILE
                );
                cfg
            }
            Err(e) => {
                eprintln!(
                    "[SensorMonitor] Warning: Failed to load {}: {}",
                    SENSOR_CONFIG_FILE, e
                );
                eprintln!("[SensorMonitor] Using default sensor configuration");
                SensorFileConfig::default_config()
            }
        };

        // Initialize sensor reader
        let base_path = w1_base_path();
        let reader = W1DeviceReader::new(&base_path);

        // Create alarm controllers for each sensor line
        // Also create MQTT callbacks to track state for temperature updates
        let mut mqtt_callbacks: Vec<Arc<MqttAlarmCallback>> = Vec::new();

        let mut alarm_controllers: [AlarmController; 8] = (0..8)
            .map(|idx| {
                let thresholds = sensor_file_config.get_line_thresholds(idx as u8);
                let mut controller = AlarmController::new(
                    thresholds,
                    config.failure_threshold,
                    5,
                    config.warmup_threshold,
                );

                // Register logging callback for this sensor
                let logger = Arc::new(LoggingCallback::new(&format!("[Sensor {}]", idx)));
                controller.register_callback(logger);

                // Register buzzer callback for disconnected and critical states
                let buzzer = Arc::new(BuzzerCallback::new(&format!("[Sensor {} Buzzer]", idx)));
                controller.register_callback(buzzer);

                // Register MQTT callback for publishing alarm state transitions
                if let Some(ref mqtt) = mqtt_handle {
                    let name = sensor_file_config
                        .lines
                        .get(idx)
                        .map(|l| l.name.clone())
                        .unwrap_or_else(|| format!("Sensor {}", idx + 1));
                    let mqtt_callback =
                        Arc::new(MqttAlarmCallback::new(mqtt.clone(), idx as u8, name));
                    controller.register_callback(mqtt_callback.clone());
                    mqtt_callbacks.push(mqtt_callback);
                }

                controller
            })
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();

        // Load buzzer timings and patterns from config
        let buzzer_critical_timing = sensor_file_config
            .alarm_patterns
            .as_ref()
            .map(|p| p.buzzer_critical_timing.clone())
            .unwrap_or_default();
        let buzzer_disconnected_timing = sensor_file_config
            .alarm_patterns
            .as_ref()
            .map(|p| p.buzzer_disconnected_timing.clone())
            .unwrap_or_default();

        // Load alarm patterns for buzzer configuration (to use enabled/pattern fields)
        let alarm_patterns = sensor_file_config
            .alarm_patterns
            .as_ref()
            .map(|p| (p.critical.clone(), p.disconnected.clone()));

        // Alarm type tracking - only reset buzzer timer when alarm type actually changes
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum AlarmType {
            None,
            Disconnected,
            Critical,
            Reconnecting,
        }

        let mut current_alarm_type = AlarmType::None;
        // Whether the previous cycle was skipped for standby, so the entry
        // clean-up runs once per standby rather than every 500 ms.
        let mut was_standby = false;
        let mut happy_beep_played = false; // Track if we've already played happy beep for this reconnection
        let mut happy_beep_start_time: Option<Instant> = None; // Track when happy beep started to auto-clear it
        let mut last_sensor_states: [Option<AlarmState>; 8] = [None; 8]; // Track previous states to detect reconnection
        let mut consecutive_failures: [u32; 8] = [0; 8]; // Track consecutive failures per sensor for debouncing

        let mut update_interval = Duration::from_secs(config.sample_interval_s);
        let failure_debounce_count = 2; // Require 2 consecutive failures before marking sensor as failed

        // Initialize aggregation state for MQTT reporting
        let pending_path = std::path::Path::new("/data/fiber/pending_aggregations.json");
        let aggregation_state = Arc::new(RwLock::new(AggregationState::new(Duration::from_secs(
            config.aggregation_interval_s,
        ))));

        // Load any pending periods saved from a previous run
        let recovered_periods = AggregationState::load_pending(pending_path);
        if !recovered_periods.is_empty() {
            if let Ok(mut agg) = aggregation_state.write() {
                agg.prepend_periods(recovered_periods);
            }
        }

        let mut last_pending_save = Instant::now();
        let pending_save_interval = Duration::from_secs(60); // Save pending every 60s

        // Track current interval values for hot-reload comparison
        let mut current_sample_interval_s = config.sample_interval_s;
        let mut current_aggregation_interval_s = config.aggregation_interval_s;

        // The alarm state last written to `sensor_readings` per line: a raw row
        // is stored only when it changes (plan/control/storage_intervals.md §4).
        let mut last_stored_states: [Option<AlarmState>; 8] = [None; 8];
        // This cycle's samples, published live on `sensors/summary`.
        let mut live = live_window(unix_now());

        // Config hot reload check interval
        let mut last_config_check = Instant::now();
        let config_check_interval = Duration::from_secs(10);
        let mut current_sensor_file_config = sensor_file_config.clone();

        // Initialize sensor names, locations and thresholds from config
        {
            let names: [String; 8] = sensor_file_config
                .lines
                .iter()
                .take(8)
                .map(|l| l.name.clone())
                .chain(std::iter::repeat("Unknown".to_string()))
                .take(8)
                .collect::<Vec<_>>()
                .try_into()
                .unwrap_or_else(|_| {
                    [
                        "Sensor 1".to_string(),
                        "Sensor 2".to_string(),
                        "Sensor 3".to_string(),
                        "Sensor 4".to_string(),
                        "Sensor 5".to_string(),
                        "Sensor 6".to_string(),
                        "Sensor 7".to_string(),
                        "Sensor 8".to_string(),
                    ]
                });

            let locations: [Option<String>; 8] = sensor_file_config
                .lines
                .iter()
                .take(8)
                .map(|l| l.location.clone())
                .chain(std::iter::repeat(None))
                .take(8)
                .collect::<Vec<_>>()
                .try_into()
                .unwrap_or_else(|_| [None, None, None, None, None, None, None, None]);

            // Collect thresholds for each sensor
            let thresholds: [crate::libs::alarms::AlarmThreshold; 8] = (0..8)
                .map(|idx| sensor_file_config.get_line_thresholds(idx as u8))
                .collect::<Vec<_>>()
                .try_into()
                .unwrap();

            if let Ok(mut state) = sensor_state.write() {
                state.set_names(names);
                state.set_locations(locations);
                state.set_thresholds(thresholds);
            }
        }

        eprintln!(
            "[SensorMonitor] Started monitoring: sample every {}s (live on MQTT), \
             storage window {}s, {} failure threshold",
            config.sample_interval_s, config.aggregation_interval_s, config.failure_threshold
        );

        // Main monitoring loop
        loop {
            // Track cycle start time to calculate accurate sleep duration
            let cycle_start = Instant::now();

            // Check for shutdown signal
            if shutdown_flag.load(Ordering::Relaxed) {
                eprintln!(
                    "[SensorMonitor] Shutdown signal received, saving pending aggregations..."
                );
                if let Ok(agg) = aggregation_state.read() {
                    if let Err(e) = agg.save_pending(pending_path) {
                        eprintln!("[SensorMonitor] Warning: Failed to save pending periods on shutdown: {}", e);
                    }
                }
                eprintln!("[SensorMonitor] Exiting monitor thread");
                break;
            }

            // In standby the sensor rails (P0..P7) are switched off, so every
            // line would read as disconnected — raising eight DISCONNECTED
            // alarms, beeping, and lighting eight red LEDs on a device the
            // operator has just switched off. Skip the whole cycle: no reads, no
            // alarm evaluation, no storage writes, no publishing.
            if crate::libs::power::standby::is_standby() {
                // On the way in, hand back everything this loop was asserting.
                // Skipping the cycle alone is not enough: this loop is the only
                // writer of line LED state and the only thing that clears the
                // buzzer's sensor-critical flag, so without this the LEDs freeze
                // showing the sensor status of a device that has stopped
                // measuring, and a unit switched off mid-alarm keeps beeping.
                //
                // `was_standby` starts false, so a boot straight into standby
                // takes this path too rather than needing its own.
                if !was_standby {
                    was_standby = true;
                    eprintln!("[SensorMonitor] Standby: clearing line LEDs and alarm state");

                    quiesce_for_standby(&led_state, &mut alarm_controllers);

                    // Clear the flag *and* the tracker. The tracker is what gates
                    // the update at :499, so resetting it is what lets the edge
                    // re-fire on resume — clearing the flag alone would leave a
                    // device that wakes genuinely in alarm, and silent.
                    //
                    // Not part of quiesce_for_standby because BuzzerPriorityManager
                    // needs a GPIO-backed BuzzerController, which no test can build.
                    priority_manager.set_sensor_critical(false);
                    current_alarm_type = AlarmType::None;
                }
                thread::sleep(Duration::from_millis(500));
                continue;
            }
            was_standby = false;

            // Hot reload sensor configuration
            if last_config_check.elapsed() >= config_check_interval {
                last_config_check = Instant::now();

                match SensorFileConfig::load_default() {
                    Ok(new_config) => {
                        // Update sensor configuration
                        current_sensor_file_config = new_config.clone();

                        // Update alarm thresholds for each sensor
                        for (idx, controller) in alarm_controllers.iter_mut().enumerate() {
                            let new_thresholds = new_config.get_line_thresholds(idx as u8);
                            controller.update_thresholds(new_thresholds);
                        }

                        // Update sensor names
                        let names: [String; 8] = new_config
                            .lines
                            .iter()
                            .take(8)
                            .map(|l| l.name.clone())
                            .chain(std::iter::repeat("Unknown".to_string()))
                            .take(8)
                            .collect::<Vec<_>>()
                            .try_into()
                            .unwrap_or_else(|_| {
                                [
                                    "Sensor 1".to_string(),
                                    "Sensor 2".to_string(),
                                    "Sensor 3".to_string(),
                                    "Sensor 4".to_string(),
                                    "Sensor 5".to_string(),
                                    "Sensor 6".to_string(),
                                    "Sensor 7".to_string(),
                                    "Sensor 8".to_string(),
                                ]
                            });

                        let locations: [Option<String>; 8] = new_config
                            .lines
                            .iter()
                            .take(8)
                            .map(|l| l.location.clone())
                            .chain(std::iter::repeat(None))
                            .take(8)
                            .collect::<Vec<_>>()
                            .try_into()
                            .unwrap_or_else(|_| [None, None, None, None, None, None, None, None]);

                        // Update thresholds for display
                        let thresholds: [crate::libs::alarms::AlarmThreshold; 8] = (0..8)
                            .map(|idx| new_config.get_line_thresholds(idx as u8))
                            .collect::<Vec<_>>()
                            .try_into()
                            .unwrap();

                        if let Ok(mut state) = sensor_state.write() {
                            state.set_names(names.clone());
                            state.set_locations(locations);
                            state.set_thresholds(thresholds);
                        }

                        // Update MQTT callback names
                        for (idx, name) in names.iter().enumerate() {
                            if let Some(mqtt_cb) = mqtt_callbacks.get(idx) {
                                mqtt_cb.set_name(name.clone());
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "[SensorMonitor] Warning: Failed to reload sensor config: {}",
                            e
                        );
                    }
                }

                // Also hot-reload main config for interval changes
                match Config::load_default() {
                    Ok(main_config) => {
                        let new_sample = main_config.sensors.sample_interval_s;
                        let new_aggregation = main_config.sensors.aggregation_interval_s;

                        if new_sample != current_sample_interval_s
                            || new_aggregation != current_aggregation_interval_s
                        {
                            eprintln!(
                                "[SensorMonitor] Hot-reloading intervals: sample={}s->{}s, aggregation={}s->{}s",
                                current_sample_interval_s, new_sample,
                                current_aggregation_interval_s, new_aggregation,
                            );

                            update_interval = Duration::from_secs(new_sample);
                            // Only a new storage window touches the open
                            // one: it is closed and stored, not dropped. A
                            // new sample interval just changes the cadence.
                            if new_aggregation != current_aggregation_interval_s {
                                if let Ok(mut agg) = aggregation_state.write() {
                                    agg.update_interval(Duration::from_secs(new_aggregation));
                                }
                            }

                            current_sample_interval_s = new_sample;
                            current_aggregation_interval_s = new_aggregation;

                            eprintln!("[SensorMonitor] Intervals updated successfully");
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "[SensorMonitor] Warning: Failed to reload main config: {}",
                            e
                        );
                    }
                }
            }

            // Check for MQTT reconnect flag (immediate flush) or regular timer
            let force_flush = mqtt_handle
                .as_ref()
                .map(|mqtt| mqtt.reconnected_flag.swap(false, Ordering::AcqRel))
                .unwrap_or(false);

            if force_flush {
                eprintln!("[SensorMonitor] MQTT reconnected - flushing buffered aggregation data immediately");
            }

            // A closed storage window is stored and published as soon as the
            // loop sees it; `force_flush` only matters for periods buffered
            // while MQTT was down, which the same drain picks up.
            let periods = if let Ok(mut agg_state) = aggregation_state.write() {
                agg_state.take_completed_periods() // Drain queue once
            } else {
                Vec::new()
            };
            if !periods.is_empty() {
                if let Some(ref storage) = storage_handle {
                    let rows: Vec<_> = periods.iter().flat_map(bucket_rows).collect();
                    if let Err(e) = storage.write_sensor_buckets(rows) {
                        eprintln!("[SensorMonitor] Failed to queue storage buckets: {}", e);
                    }
                }

                if let Some(ref mqtt) = mqtt_handle {
                    let (names, locations) = names_and_locations(&sensor_state);
                    if force_flush && !periods.is_empty() {
                        eprintln!(
                            "[SensorMonitor] Flushing {} buffered periods after reconnect",
                            periods.len()
                        );
                    }
                    let published_count = periods.len();
                    for period in periods {
                        mqtt.send_aggregated_sensor_data(period, names.clone(), locations.clone());
                    }
                    // Clear pending file after successful publish
                    if published_count > 0 {
                        let _ = std::fs::remove_file(pending_path);
                    }
                }
            }

            // Periodically save pending aggregation periods for crash recovery
            if last_pending_save.elapsed() >= pending_save_interval {
                last_pending_save = Instant::now();
                if let Ok(agg) = aggregation_state.read() {
                    if let Err(e) = agg.save_pending(pending_path) {
                        eprintln!(
                            "[SensorMonitor] Warning: Failed to save pending periods: {}",
                            e
                        );
                    }
                }
            }

            // Determine buzzer pattern based on sensor states
            // The dedicated buzzer thread will handle the actual timing and pin control
            {
                // Determine the current alarm type (what alarm condition exists)
                let mut new_alarm_type = AlarmType::None;
                let mut has_critical = false;
                let mut has_disconnected = false;
                let mut has_just_reconnected = false;

                // Check if any sensor is in critical, reconnecting, or disconnected state
                for (idx, controller) in alarm_controllers.iter().enumerate() {
                    let current_state = controller.state();

                    // Check if sensor just transitioned to Reconnecting (Disconnected → Reconnecting)
                    if controller.just_reconnecting() {
                        has_just_reconnected = true;
                    }

                    // Also check if sensor transitioned from DISCONNECTED/RECONNECTING to any valid state
                    // (NORMAL, Warning, or ALARM) - this means it successfully reconnected
                    if let Some(last_state) = last_sensor_states[idx] {
                        if last_state == AlarmState::Disconnected
                            || last_state == AlarmState::Reconnecting
                        {
                            match current_state {
                                AlarmState::Normal | AlarmState::Warning | AlarmState::Critical => {
                                    // Successfully reconnected to a valid measurement state
                                    has_just_reconnected = true;
                                }
                                _ => {}
                            }
                        }
                    }

                    // Detect per-sensor transition into critical/disconnected
                    // to bust button silence when a NEW sensor alarms
                    // NOTE: must run BEFORE the break below so all sensors are checked
                    if let Some(last_state) = last_sensor_states[idx] {
                        let was_critical_like =
                            matches!(last_state, AlarmState::Critical | AlarmState::Disconnected);
                        let is_critical_like = matches!(
                            current_state,
                            AlarmState::Critical | AlarmState::Disconnected
                        );
                        if !was_critical_like && is_critical_like {
                            priority_manager.on_new_sensor_alarm();
                            eprintln!("[SensorMonitor] New sensor alarm detected (sensor {}), silence cleared", idx);
                        }
                    }

                    // Update last state for this sensor (before potential break)
                    last_sensor_states[idx] = Some(current_state);

                    match current_state {
                        AlarmState::Critical => {
                            has_critical = true;
                        }
                        AlarmState::Disconnected => {
                            has_disconnected = true;
                        }
                        _ => {}
                    }
                }

                // Play happy beep once when transitioning from Disconnected → Connected
                if has_just_reconnected && !happy_beep_played {
                    happy_beep_played = true;
                    happy_beep_start_time = Some(Instant::now());
                    eprintln!("[SensorMonitor] Sensor reconnected! Playing happy beep...");
                    if let Ok(mut bz) = buzzer.lock() {
                        bz.play_once(BuzzerPattern::ReconnectionHappy { frequency_hz: 50 });
                    }
                } else if !has_just_reconnected {
                    // Reset happy beep flag when no longer in reconnecting state
                    happy_beep_played = false;

                    // Auto-clear happy beep after 1.5 seconds
                    if let Some(start_time) = happy_beep_start_time {
                        if start_time.elapsed().as_millis() >= 1500 {
                            happy_beep_start_time = None;
                            eprintln!(
                                "[SensorMonitor] Happy beep duration expired, clearing pattern"
                            );
                            if let Ok(mut bz) = buzzer.lock() {
                                bz.stop();
                            }
                        }
                    } else {
                        // Normal alarm type detection (when happy beep is not playing)
                        if has_critical {
                            new_alarm_type = AlarmType::Critical;
                        } else if has_disconnected {
                            new_alarm_type = AlarmType::Disconnected;
                        }

                        // Check if button silence has expired — resume beeping if so
                        priority_manager.check_silence_expiry();

                        // Only update shared state when alarm type actually changes
                        // This prevents resets from sensor state oscillations (NORMAL ↔ DISCONNECTED)
                        if new_alarm_type != current_alarm_type {
                            current_alarm_type = new_alarm_type;

                            // Update priority manager to coordinate with battery critical alarms
                            match new_alarm_type {
                                AlarmType::Critical | AlarmType::Disconnected => {
                                    // Sensor has critical or disconnected alarm
                                    eprintln!(
                                        "[SensorMonitor] Sensor alarm type changed: {:?}",
                                        new_alarm_type
                                    );
                                    priority_manager.set_sensor_critical(true);
                                }
                                AlarmType::None => {
                                    // All clear - no sensor alarms
                                    eprintln!("[SensorMonitor] Sensor alarms cleared");
                                    priority_manager.set_sensor_critical(false);
                                }
                                AlarmType::Reconnecting => {
                                    // Reconnecting - don't affect critical state
                                }
                            }
                        }
                    }
                }
            }

            // Enumerate available W1 devices
            match reader.enum_devices() {
                Ok(devices) => {
                    // Filter devices to only those within configured line count
                    let devices_to_read: Vec<_> = devices
                        .iter()
                        .filter(|(line_num, _)| (*line_num as usize) < config.num_lines as usize)
                        .collect();

                    // Read all sensors in PARALLEL using thread::scope
                    // Since each sensor is on a separate 1-Wire bus (w1_bus_master1, w1_bus_master2, etc.),
                    // they can be read simultaneously, reducing total read time from ~8s to ~1s
                    let read_results: Vec<(u8, Result<f32, std::io::Error>)> = thread::scope(|s| {
                        let handles: Vec<_> = devices_to_read
                            .iter()
                            .map(|(line_num, device_id)| {
                                let reader_ref = &reader;
                                let line = *line_num;
                                let dev_id = device_id.clone();
                                s.spawn(move || {
                                    (line, reader_ref.read_temperature(line, &dev_id, 3000))
                                })
                            })
                            .collect();

                        handles
                            .into_iter()
                            .map(
                                |h: thread::ScopedJoinHandle<
                                    '_,
                                    (u8, Result<f32, std::io::Error>),
                                >| {
                                    h.join().expect("Sensor read thread panicked")
                                },
                            )
                            .collect()
                    });

                    // Process results sequentially (alarm controllers and state updates aren't thread-safe)
                    for (line_num, result) in read_results {
                        let sensor_idx = line_num as usize;

                        match result {
                            Ok(temp) => {
                                // Successful read - update alarm controller and reset failure counter
                                let _was_disconnected =
                                    consecutive_failures[sensor_idx] >= failure_debounce_count;

                                // Update MQTT callback temperature BEFORE updating alarm controller
                                // so that state change events have the correct temperature
                                if let Some(mqtt_cb) = mqtt_callbacks.get(sensor_idx) {
                                    mqtt_cb.set_temperature(temp);
                                }

                                alarm_controllers[sensor_idx].update(temp);
                                consecutive_failures[sensor_idx] = 0;

                                // Update shared sensor state for display with current alarm state
                                let alarm_state = alarm_controllers[sensor_idx].state();
                                if let Ok(mut state) = sensor_state.write() {
                                    state.set_reading(
                                        sensor_idx as u8,
                                        SensorReading::new(temp, true, alarm_state),
                                    );
                                }

                                record_sample(
                                    &aggregation_state,
                                    &mut live,
                                    storage_handle.as_ref(),
                                    &mut last_stored_states,
                                    sensor_idx,
                                    temp,
                                    true,
                                    alarm_state,
                                );
                            }
                            Err(e) => {
                                // Track consecutive failures with debouncing
                                consecutive_failures[sensor_idx] += 1;
                                eprintln!(
                                    "[SensorMonitor] Error reading sensor {} (failure {}): {}",
                                    sensor_idx, consecutive_failures[sensor_idx], e
                                );
                                // Only mark as failed after multiple consecutive failures
                                if consecutive_failures[sensor_idx] >= failure_debounce_count {
                                    alarm_controllers[sensor_idx].mark_read_failure();
                                    // Mark as disconnected in shared sensor state
                                    let alarm_state = alarm_controllers[sensor_idx].state();
                                    if let Ok(mut state) = sensor_state.write() {
                                        state.set_reading(
                                            sensor_idx as u8,
                                            SensorReading::new(0.0, false, alarm_state),
                                        );
                                    }

                                    record_sample(
                                        &aggregation_state,
                                        &mut live,
                                        storage_handle.as_ref(),
                                        &mut last_stored_states,
                                        sensor_idx,
                                        0.0,
                                        false,
                                        alarm_state,
                                    );
                                }
                            }
                        }
                    }

                    // Mark the lines without a device as not found (disconnected) -
                    // requires debouncing too
                    for sensor_idx in absent_lines(&devices, config.num_lines) {
                        consecutive_failures[sensor_idx] += 1;
                        if consecutive_failures[sensor_idx] >= failure_debounce_count {
                            alarm_controllers[sensor_idx].mark_read_failure();
                            // Mark as disconnected in shared sensor state
                            let alarm_state = alarm_controllers[sensor_idx].state();
                            if let Ok(mut state) = sensor_state.write() {
                                state.set_reading(
                                    sensor_idx as u8,
                                    SensorReading::new(0.0, false, alarm_state),
                                );
                            }

                            record_sample(
                                &aggregation_state,
                                &mut live,
                                storage_handle.as_ref(),
                                &mut last_stored_states,
                                sensor_idx,
                                0.0,
                                false,
                                alarm_state,
                            );
                        }
                    }
                }
                Err(e) => {
                    // A missing base directory and a failed read of an existing one mean
                    // very different things: off-device there is simply no 1-Wire bus,
                    // while on a Hub it means no master registered — the DS2482 bridge
                    // on i2c-10 is not there. Either way, say it once rather than every
                    // sample cycle.
                    static BUS_ABSENT_LOGGED: AtomicBool = AtomicBool::new(false);
                    static BUS_ABSENT_LOG: AtomicU64 = AtomicU64::new(0);
                    static ENUM_ERROR_LOG: AtomicU64 = AtomicU64::new(0);

                    if !reader.bus_present() {
                        // Once on a dev build; on a Hub this is a fault, so keep
                        // reminding at a readable rate.
                        let first = !BUS_ABSENT_LOGGED.swap(true, Ordering::Relaxed);
                        let should_log = if cfg!(feature = "dev-platform") {
                            first
                        } else {
                            first || throttle(&BUS_ABSENT_LOG, 60)
                        };
                        if should_log {
                            // Name the master this build actually expects. The Hub's is
                            // a DS2482 I2C bridge described by proximos.dtso; w1-gpio is
                            // the dev platform's bit-banged master and points a Hub
                            // operator at hardware their board does not have.
                            let hint = if cfg!(feature = "dev-platform") {
                                "w1-gpio overlay not loaded?"
                            } else {
                                "no 1-Wire master registered — check the DS2482 bridge \
                                 on i2c-10 (expected 10-0018) and that the ds2482 module \
                                 loaded"
                            };
                            eprintln!(
                                "[SensorMonitor] 1-Wire bus not present at {} — temperature \
                                 monitoring is offline ({})",
                                base_path, hint
                            );
                        }
                    } else if throttle(&ENUM_ERROR_LOG, 60) {
                        eprintln!("[SensorMonitor] Error enumerating W1 devices: {}", e);
                    }

                    // On enumeration error, track failure for all sensors with debouncing.
                    // Unconditional on purpose: a line that had been reading must still go
                    // Disconnected when the bus disappears under it, on every build.
                    for idx in 0..config.num_lines as usize {
                        consecutive_failures[idx] += 1;
                        if consecutive_failures[idx] >= failure_debounce_count {
                            alarm_controllers[idx].mark_read_failure();
                        }
                    }
                }
            }

            // Publish this cycle's samples live on `sensors/summary`, then start
            // the next cycle. Nothing here is stored: the storage window does that.
            let cycle_end_ts = unix_now();
            if let Some(ref mqtt) = mqtt_handle {
                if live
                    .iter()
                    .any(|w| w.sample_count + w.disconnected_count > 0)
                {
                    let (names, locations) = names_and_locations(&sensor_state);
                    let start_ts = live[0].window_start_ts;
                    let mut sensors = live.clone();
                    for w in sensors.iter_mut() {
                        w.finalize(cycle_end_ts);
                    }
                    mqtt.send_sensor_summary(
                        AggregationPeriod::new(sensors, start_ts, cycle_end_ts),
                        names,
                        locations,
                    );
                }
            }
            live = live_window(cycle_end_ts);

            // Check if aggregation window should be finalized
            if let Ok(mut agg_state) = aggregation_state.write() {
                if agg_state.should_finalize_window() {
                    agg_state.finalize_window();
                }
            }

            // Update LED state in shared state (actual LED control happens in dedicated LedMonitor thread)
            // The set_line_led() method automatically notifies the LED monitor of changes
            for (idx, controller) in alarm_controllers.iter().enumerate() {
                let led = controller.get_led_state();
                led_state.set_line_led(idx as u8, led);
            }

            // Progress reconnection animations for all sensors
            for controller in alarm_controllers.iter_mut() {
                controller.advance_reconnect_animation();
            }

            // Calculate remaining sleep time to achieve target sample interval
            // The sample_interval_ms represents the total cycle time (read + sleep),
            // not just the sleep time. This accounts for 1-Wire sensor conversion time.
            let cycle_duration = cycle_start.elapsed();
            let sleep_time = update_interval.saturating_sub(cycle_duration);

            if sleep_time.is_zero() && update_interval.as_millis() > 0 {
                // Only warn once per minute to avoid log spam
                static LAST_WARNING: AtomicU64 = AtomicU64::new(0);
                if throttle(&LAST_WARNING, 60) {
                    eprintln!(
                        "[SensorMonitor] Warning: Sensor read cycle took {:?}, exceeds target interval {:?}",
                        cycle_duration, update_interval
                    );
                }
            }

            thread::sleep(sleep_time);
        }

        eprintln!("[SensorMonitor] Monitor thread exited cleanly");
    }

    /// Gracefully shutdown the monitoring thread
    pub fn shutdown(mut self) -> io::Result<()> {
        // Signal sensor thread to shutdown
        self.shutdown_flag.store(true, Ordering::Relaxed);

        // Wait for main sensor thread to finish
        if let Some(handle) = self.thread_handle.take() {
            handle.join().ok();
        }

        // Note: The buzzer controller will be properly shutdown via its Drop impl
        // when self is dropped at the end of this function

        Ok(())
    }
}

/// Hand back what the sensor loop was asserting, on the way into standby.
///
/// Extracted from the loop so the user-visible half is testable — the loop itself
/// needs a 1-Wire bus, an STM bridge and a GPIO-backed buzzer.
///
/// Both halves matter:
///
/// * The line LEDs go to an explicit `Off`. This loop is the only writer of line
///   LED state, so a skipped cycle leaves them frozen showing the sensor status of
///   a device that has stopped measuring. It has to be `Off` and not `None` —
///   [`crate::libs::leds::monitor`] skips `None` lines, so clearing to `None` would
///   leave them lit — and it has to go through the shared state rather than the STM
///   bridge, or the monitor's cache would still match the pre-standby values and
///   the LEDs would never come back on resume.
/// * The controllers forget what they saw. The rails are switched off in standby,
///   so a latched `Critical` or `Disconnected` is an assertion nothing can justify.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The configured lines with no device on their bus. Lines are the bus
/// masters' (0-based) numbers, not positions in the list: with sensors on
/// lines 0, 2 and 4, line 1 is absent, and lines 2 and 4 are not.
fn absent_lines(devices: &[(u8, String)], num_lines: u8) -> Vec<usize> {
    (0..num_lines.min(8) as usize)
        .filter(|&line| !devices.iter().any(|(l, _)| *l as usize == line))
        .collect()
}

/// An empty per-line window starting at `start_ts`.
fn live_window(start_ts: u64) -> [SensorAggregation; 8] {
    std::array::from_fn(|line| {
        let mut w = SensorAggregation::new(line as u8);
        w.reset(start_ts);
        w
    })
}

/// Sensor names and locations for MQTT documents, with numbered fallbacks.
fn names_and_locations(
    sensor_state: &super::state::SharedSensorStateHandle,
) -> ([String; 8], [Option<String>; 8]) {
    sensor_state
        .read()
        .map(|s| (s.names.clone(), s.locations.clone()))
        .unwrap_or_else(|_| {
            (
                std::array::from_fn(|i| format!("Sensor {}", i + 1)),
                Default::default(),
            )
        })
}

/// One sample of one line: into the storage window, into the live cycle, and
/// into `sensor_readings` only when the line's alarm state changed — the
/// sample that raised or cleared an alarm is kept at once
/// (plan/control/storage_intervals.md §4, S2). The first sample after start
/// is a change from "nothing stored yet" and gives each line a baseline row.
#[allow(clippy::too_many_arguments)]
fn record_sample(
    aggregation_state: &RwLock<AggregationState>,
    live: &mut [SensorAggregation; 8],
    storage: Option<&StorageHandle>,
    last_stored_states: &mut [Option<AlarmState>; 8],
    line: usize,
    temperature: f32,
    is_connected: bool,
    alarm_state: AlarmState,
) {
    if line >= 8 {
        return;
    }
    if let Ok(mut agg_state) = aggregation_state.write() {
        // A sample past the window end opens the next window, it does not
        // extend the closed one.
        agg_state.roll_over_if_due();
        agg_state.add_reading(line as u8, temperature, is_connected, alarm_state);
    }
    live[line].add_sample(temperature, is_connected, alarm_state, None);

    if last_stored_states[line] == Some(alarm_state) {
        return;
    }
    if let Some(storage) = storage {
        if storage
            .write_sensor_reading(
                unix_now() as i64,
                line as u8,
                temperature,
                is_connected,
                alarm_state,
            )
            .is_ok()
        {
            last_stored_states[line] = Some(alarm_state);
        }
    }
}

/// The `sensor_readings_minute` rows of one closed storage window: one per
/// line that had any sample, keyed by the window start.
fn bucket_rows(period: &AggregationPeriod) -> Vec<MinuteAggregateRow> {
    let created_at = unix_now() as i64;
    period
        .sensors
        .iter()
        .filter(|w| w.sample_count + w.disconnected_count > 0)
        .map(|w| {
            let (min_c, avg_c, max_c) = if w.sample_count > 0 {
                (
                    w.min_temp_celsius as f64,
                    w.avg_temp_celsius as f64,
                    w.max_temp_celsius as f64,
                )
            } else {
                (0.0, 0.0, 0.0)
            };
            MinuteAggregateRow {
                minute_ts: period.period_start_ts as i64,
                sensor_line: w.line,
                min_c,
                avg_c,
                max_c,
                sample_count: (w.sample_count + w.disconnected_count) as i64,
                disconnect_count: w.disconnected_count as i64,
                worst_alarm: worst_alarm(&w.alarm_counts).to_string(),
                created_at,
                data_hmac: None,
                window_s: Some(period.period_end_ts.saturating_sub(period.period_start_ts) as i64),
            }
        })
        .collect()
}

/// The storage layer's severity order (CRITICAL > DISCONNECTED > WARNING >
/// RECONNECTING > NORMAL), which ranks a lost sensor above a warning — unlike
/// `AlarmStateCounts::dominant`, which serves the MQTT document.
fn worst_alarm(counts: &super::aggregation::AlarmStateCounts) -> &'static str {
    if counts.critical > 0 {
        "CRITICAL"
    } else if counts.disconnected > 0 {
        "DISCONNECTED"
    } else if counts.warning > 0 {
        "WARNING"
    } else if counts.reconnecting > 0 {
        "RECONNECTING"
    } else {
        "NORMAL"
    }
}

fn quiesce_for_standby(
    led_state: &SharedLedStateHandle,
    alarm_controllers: &mut [AlarmController],
) {
    led_state.set_all_lines_off();
    for controller in alarm_controllers.iter_mut() {
        controller.reset();
    }
}

#[cfg(test)]
mod storage_window_tests {
    use super::*;

    fn period_with(samples: &[(u8, f32, bool, AlarmState)]) -> AggregationPeriod {
        let mut sensors = live_window(1_800);
        for &(line, t, connected, state) in samples {
            sensors[line as usize].add_sample(t, connected, state, None);
        }
        AggregationPeriod::new(sensors, 1_800, 2_400)
    }

    #[test]
    fn one_bucket_per_line_with_samples_keyed_by_the_window_start() {
        let rows = bucket_rows(&period_with(&[
            (0, 4.0, true, AlarmState::Normal),
            (0, 6.0, true, AlarmState::Warning),
            (2, 0.0, false, AlarmState::Disconnected),
        ]));
        assert_eq!(rows.len(), 2);
        let l0 = &rows[0];
        assert_eq!((l0.minute_ts, l0.sensor_line), (1_800, 0));
        assert_eq!(l0.window_s, Some(600));
        assert_eq!((l0.min_c, l0.avg_c, l0.max_c), (4.0, 5.0, 6.0));
        assert_eq!((l0.sample_count, l0.disconnect_count), (2, 0));
        assert_eq!(l0.worst_alarm, "WARNING");
        let l2 = &rows[1];
        assert_eq!(
            (l2.sensor_line, l2.sample_count, l2.disconnect_count),
            (2, 1, 1)
        );
        assert_eq!(l2.worst_alarm, "DISCONNECTED");
    }

    #[test]
    fn a_lost_sensor_outranks_a_warning_in_storage() {
        let rows = bucket_rows(&period_with(&[
            (1, 9.0, true, AlarmState::Warning),
            (1, 0.0, false, AlarmState::Disconnected),
        ]));
        assert_eq!(rows[0].worst_alarm, "DISCONNECTED");
        let rows = bucket_rows(&period_with(&[
            (1, 0.0, false, AlarmState::Disconnected),
            (1, 30.0, true, AlarmState::Critical),
        ]));
        assert_eq!(rows[0].worst_alarm, "CRITICAL");
    }

    /// Every sample feeds the window and the live cycle; with no storage
    /// handle nothing is marked stored, so the first real write still happens.
    #[test]
    fn every_sample_feeds_the_window_and_the_live_cycle() {
        let agg = RwLock::new(AggregationState::new(Duration::from_secs(600)));
        let mut live = live_window(0);
        let mut stored = [None; 8];
        for t in [4.0, 5.0] {
            record_sample(
                &agg,
                &mut live,
                None,
                &mut stored,
                3,
                t,
                true,
                AlarmState::Normal,
            );
        }
        assert_eq!(live[3].sample_count, 2);
        assert_eq!(agg.read().unwrap().current_windows_sample_count(3), 2);
        assert_eq!(stored[3], None);
    }

    /// The first sample after a window boundary closes the old window
    /// before it is counted, so it lands in the new window.
    #[test]
    fn a_sample_past_the_window_end_lands_in_the_next_window() {
        let agg = RwLock::new(AggregationState::new(Duration::from_secs(600)));
        let now = unix_now();
        let old_start = (now / 600) * 600 - 600;
        agg.write().unwrap().set_window_start_for_test(old_start);
        let mut live = live_window(0);
        let mut stored = [None; 8];
        record_sample(
            &agg,
            &mut live,
            None,
            &mut stored,
            2,
            6.0,
            true,
            AlarmState::Normal,
        );
        let agg = agg.read().unwrap();
        let periods = agg.completed_periods_for_test();
        assert_eq!(periods.len(), 1);
        assert_eq!(periods[0].period_start_ts, old_start);
        assert_eq!(periods[0].sensors[2].sample_count, 0);
        assert_eq!(agg.current_windows_sample_count(2), 1);
    }
}

#[cfg(test)]
mod w1_path_tests {
    use super::*;

    // Mutates process-wide env; the crate's tests already run with --test-threads=1.
    #[test]
    fn w1_base_path_defaults_to_sysfs_and_honours_the_override() {
        std::env::remove_var("FIBER_W1_BASE_PATH");
        assert_eq!(w1_base_path(), W1_BASE_PATH_DEFAULT);

        std::env::set_var("FIBER_W1_BASE_PATH", "/tmp/fake-w1");
        assert_eq!(w1_base_path(), "/tmp/fake-w1");

        std::env::remove_var("FIBER_W1_BASE_PATH");
        assert_eq!(w1_base_path(), W1_BASE_PATH_DEFAULT);
    }

    #[test]
    fn throttle_fires_once_then_suppresses_within_the_period() {
        // A fresh slot always fires, so the first occurrence is never swallowed.
        let slot = AtomicU64::new(0);
        assert!(throttle(&slot, 60));
        assert!(!throttle(&slot, 60));
        assert!(!throttle(&slot, 60));
    }

    #[test]
    fn absent_lines_are_the_gaps_not_the_tail_of_the_device_list() {
        let devices = vec![
            (0, "28-a".to_string()),
            (2, "28-b".to_string()),
            (4, "28-c".to_string()),
        ];
        assert_eq!(absent_lines(&devices, 8), vec![1, 3, 5, 6, 7]);
        assert_eq!(absent_lines(&devices, 3), vec![1]);
        assert_eq!(absent_lines(&[], 4), vec![0, 1, 2, 3]);
    }

    #[test]
    fn throttle_with_a_zero_period_always_fires() {
        let slot = AtomicU64::new(0);
        assert!(throttle(&slot, 0));
        assert!(throttle(&slot, 0));
    }
}

#[cfg(test)]
mod standby_tests {
    use super::*;
    use crate::libs::alarms::color::{BlinkPattern, LedColor};
    use crate::libs::alarms::threshold::AlarmThreshold;
    use crate::libs::leds::state::SharedLedStateWithNotify;

    fn controllers(n: usize) -> Vec<AlarmController> {
        (0..n)
            .map(|_| AlarmController::new(AlarmThreshold::default_medical(), 3, 5, 1))
            .collect()
    }

    #[test]
    fn standby_darkens_every_line_led() {
        // The reported problem: the LEDs kept showing sensor status on a device
        // that had been switched off.
        let led_state: SharedLedStateHandle = Arc::new(SharedLedStateWithNotify::new());
        let mut cs = controllers(8);
        for c in cs.iter_mut() {
            c.update(37.0); // green, steady
        }
        for (i, c) in cs.iter().enumerate() {
            led_state.set_line_led(i as u8, c.get_led_state());
        }
        assert!(led_state
            .read()
            .lines
            .iter()
            .all(|l| l.unwrap().led_state.color == LedColor::Green));

        quiesce_for_standby(&led_state, &mut cs);

        for line in led_state.read().lines.iter() {
            let s = line.expect("must be an explicit Off, not None");
            assert_eq!(s.led_state.color, LedColor::Off);
            assert_eq!(s.led_state.pattern, BlinkPattern::Steady);
        }
    }

    #[test]
    fn standby_drops_a_latched_critical_alarm() {
        // A device switched off mid-alarm must not carry the alarm through standby:
        // the rails are down, so nothing can confirm it, and it would keep the
        // buzzer going on a unit that looks off.
        let led_state: SharedLedStateHandle = Arc::new(SharedLedStateWithNotify::new());
        let mut cs = controllers(1);
        cs[0].update(45.0);
        assert_eq!(cs[0].state(), AlarmState::Critical);

        quiesce_for_standby(&led_state, &mut cs);

        assert_eq!(cs[0].state(), AlarmState::NeverConnected);
        assert_eq!(cs[0].get_led_state().color, LedColor::Off);
    }

    #[test]
    fn a_resume_re_alarms_if_the_reading_is_still_critical() {
        // Forgetting must not mean forgiving. Thresholds survive, so the next live
        // reading raises the alarm again.
        let led_state: SharedLedStateHandle = Arc::new(SharedLedStateWithNotify::new());
        let mut cs = controllers(1);
        cs[0].update(45.0);
        quiesce_for_standby(&led_state, &mut cs);

        cs[0].update(45.0);
        assert_eq!(cs[0].state(), AlarmState::Critical);
        assert_eq!(cs[0].get_led_state().color, LedColor::Red);
    }

    #[test]
    fn a_resume_repaints_the_leds_because_the_value_changed() {
        // Why no cache invalidation is needed in LedMonitor: the shared value goes
        // Green -> Off -> Green, and the monitor diffs against what it last sent,
        // so both transitions are visible to it.
        let led_state: SharedLedStateHandle = Arc::new(SharedLedStateWithNotify::new());
        let mut cs = controllers(1);
        cs[0].update(37.0);
        led_state.set_line_led(0, cs[0].get_led_state());

        quiesce_for_standby(&led_state, &mut cs);
        assert_eq!(
            led_state.read().lines[0].unwrap().led_state.color,
            LedColor::Off
        );

        // Resume: the loop writes all lines every cycle, and a warmed-up line is
        // green again.
        for _ in 0..2 {
            cs[0].update(37.0);
        }
        led_state.set_line_led(0, cs[0].get_led_state());
        assert_eq!(
            led_state.read().lines[0].unwrap().led_state.color,
            LedColor::Green
        );
    }

    #[test]
    fn quiescing_a_device_with_no_sensors_is_harmless() {
        // Lines that were never written are None; standby still gives them an
        // explicit Off so the monitor's cache agrees with the hardware.
        let led_state: SharedLedStateHandle = Arc::new(SharedLedStateWithNotify::new());
        let mut cs = controllers(8);

        quiesce_for_standby(&led_state, &mut cs);

        assert!(led_state.read().lines.iter().all(|l| l.is_some()));
        assert!(cs.iter().all(|c| c.state() == AlarmState::NeverConnected));
    }
}

impl Drop for SensorMonitor {
    fn drop(&mut self) {
        // Signal shutdown on drop
        self.shutdown_flag.store(true, Ordering::Relaxed);

        // Wait for main sensor thread with a timeout
        if let Some(handle) = self.thread_handle.take() {
            let timeout = Duration::from_secs(2);
            let start = std::time::Instant::now();
            while !handle.is_finished() && start.elapsed() < timeout {
                thread::sleep(Duration::from_millis(10));
            }
        }

        // Buzzer controller will be dropped and cleaned up automatically
        // via its own Drop implementation
    }
}
