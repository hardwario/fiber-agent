// Background sensor monitoring thread

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::drivers::StmBridge;
use crate::libs::config::{SensorConfig, SensorFileConfig};
use crate::libs::alarms::{AlarmController, AlarmState, LoggingCallback, BuzzerCallback};
use crate::libs::buzzer::{BuzzerController, BuzzerPattern, BuzzerPriorityManager};
use crate::libs::leds::SharedLedStateHandle;
use crate::libs::mqtt::MqttHandle;

use super::reader::W1DeviceReader;
use super::state::{SensorReading, SharedSensorStateHandle};

const W1_BASE_PATH: &str = "/sys/bus/w1/devices";
const SENSOR_CONFIG_FILE: &str = "/data/fiber/config/fiber.sensors.config.yaml";

/// Background sensor monitoring thread
pub struct SensorMonitor {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
    buzzer: Arc<Mutex<BuzzerController>>,
}

impl SensorMonitor {
    /// Create and spawn background sensor monitoring thread
    pub fn new(config: SensorConfig, stm: Arc<Mutex<StmBridge>>, led_state: SharedLedStateHandle, buzzer: Arc<Mutex<BuzzerController>>, sensor_state: SharedSensorStateHandle, priority_manager: Arc<BuzzerPriorityManager>, mqtt_handle: Option<MqttHandle>) -> io::Result<Self> {
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();

        let buzzer_clone = buzzer.clone();

        // Spawn the main sensor monitoring thread
        let thread_handle = thread::spawn(move || {
            Self::monitor_loop(config, stm, shutdown_flag_clone, buzzer_clone, led_state, sensor_state, priority_manager, mqtt_handle);
        });

        Ok(Self {
            thread_handle: Some(thread_handle),
            shutdown_flag,
            buzzer,
        })
    }

    /// Background monitoring loop
    fn monitor_loop(config: SensorConfig, stm: Arc<Mutex<StmBridge>>, shutdown_flag: Arc<AtomicBool>, buzzer: Arc<Mutex<BuzzerController>>, led_state: SharedLedStateHandle, sensor_state: SharedSensorStateHandle, priority_manager: Arc<BuzzerPriorityManager>, mqtt_handle: Option<MqttHandle>) {
        // Load sensor file configuration
        let sensor_file_config = match SensorFileConfig::load_default() {
            Ok(cfg) => {
                eprintln!("[SensorMonitor] Loaded sensor configuration from {}", SENSOR_CONFIG_FILE);
                cfg
            }
            Err(e) => {
                eprintln!("[SensorMonitor] Warning: Failed to load {}: {}", SENSOR_CONFIG_FILE, e);
                eprintln!("[SensorMonitor] Using default sensor configuration");
                SensorFileConfig::default_config()
            }
        };

        // Initialize sensor reader
        let reader = W1DeviceReader::new(W1_BASE_PATH);

        // Create alarm controllers for each sensor line
        let mut alarm_controllers: [AlarmController; 8] = (0..8)
            .map(|idx| {
                let thresholds = sensor_file_config.get_line_thresholds(idx as u8);
                let mut controller = AlarmController::new(thresholds, config.failure_threshold, 5);

                // Register logging callback for this sensor
                let logger = Arc::new(LoggingCallback::new(&format!("[Sensor {}]", idx)));
                controller.register_callback(logger);

                // Register buzzer callback for disconnected and critical states
                let buzzer = Arc::new(BuzzerCallback::new(&format!("[Sensor {} Buzzer]", idx)));
                controller.register_callback(buzzer);

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
        let alarm_patterns = sensor_file_config.alarm_patterns.as_ref().map(|p| {
            (
                p.critical.clone(),
                p.disconnected.clone(),
            )
        });

        // Alarm type tracking - only reset buzzer timer when alarm type actually changes
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum AlarmType { None, Disconnected, Critical, Reconnecting }

        let mut current_alarm_type = AlarmType::None;
        let mut happy_beep_played = false;  // Track if we've already played happy beep for this reconnection
        let mut happy_beep_start_time: Option<Instant> = None;  // Track when happy beep started to auto-clear it
        let mut last_sensor_states: [Option<AlarmState>; 8] = [None; 8];  // Track previous states to detect reconnection
        let mut consecutive_failures: [u32; 8] = [0; 8];  // Track consecutive failures per sensor for debouncing

        let update_interval = Duration::from_millis(config.sample_interval_ms);
        let failure_debounce_count = 2;  // Require 2 consecutive failures before marking sensor as failed

        eprintln!(
            "[SensorMonitor] Started monitoring with {}ms sample interval, {} failure threshold",
            config.sample_interval_ms, config.failure_threshold
        );

        // Main monitoring loop
        loop {
            // Check for shutdown signal
            if shutdown_flag.load(Ordering::Relaxed) {
                eprintln!("[SensorMonitor] Shutdown signal received, exiting monitor thread");
                break;
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
                        if last_state == AlarmState::Disconnected || last_state == AlarmState::Reconnecting {
                            match current_state {
                                AlarmState::Normal | AlarmState::Warning | AlarmState::Alarm => {
                                    // Successfully reconnected to a valid measurement state
                                    has_just_reconnected = true;
                                }
                                _ => {}
                            }
                        }
                    }

                    match current_state {
                        AlarmState::Critical => {
                            has_critical = true;
                            break;  // Critical takes priority
                        }
                        AlarmState::Disconnected => {
                            has_disconnected = true;
                        }
                        _ => {}
                    }

                    // Update last state for this sensor
                    last_sensor_states[idx] = Some(current_state);
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
                            eprintln!("[SensorMonitor] Happy beep duration expired, clearing pattern");
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

                        // Only update shared state when alarm type actually changes
                        // This prevents resets from sensor state oscillations (NORMAL ↔ DISCONNECTED)
                        if new_alarm_type != current_alarm_type {
                            current_alarm_type = new_alarm_type;

                            // Update priority manager to coordinate with battery critical alarms
                            match new_alarm_type {
                                AlarmType::Critical | AlarmType::Disconnected => {
                                    // Sensor has critical or disconnected alarm
                                    eprintln!("[SensorMonitor] Sensor alarm type changed: {:?}", new_alarm_type);
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
                    // Read each connected device
                    for (line_num, device_id) in devices.iter() {
                        let sensor_idx = *line_num as usize;
                        if sensor_idx >= config.num_lines as usize {
                            continue;
                        }

                        // Read temperature from this sensor (3 second timeout instead of 10s)
                        // Shorter timeout ensures buzzer updates happen more frequently even during sensor reads
                        match reader.read_temperature(*line_num, device_id, 3000) {
                            Ok(temp) => {
                                // Successful read - update alarm controller and reset failure counter
                                alarm_controllers[sensor_idx].update(temp);
                                consecutive_failures[sensor_idx] = 0;

                                // Update shared sensor state for display with current alarm state
                                let alarm_state = alarm_controllers[sensor_idx].state();
                                if let Ok(mut state) = sensor_state.write() {
                                    state.set_reading(sensor_idx as u8, SensorReading::new(temp, true, alarm_state));
                                }

                                // Publish to MQTT if available
                                if let Some(ref mqtt) = mqtt_handle {
                                    mqtt.send_sensor_reading(sensor_idx as u8, temp, true, alarm_state);
                                }

                                //eprintln!("[SensorMonitor] Sensor {}: {:.1}°C",sensor_idx, temp);
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
                                        state.set_reading(sensor_idx as u8, SensorReading::new(0.0, false, alarm_state));
                                    }

                                    // Publish disconnection to MQTT if available
                                    if let Some(ref mqtt) = mqtt_handle {
                                        mqtt.send_sensor_reading(sensor_idx as u8, 0.0, false, alarm_state);
                                    }
                                }
                            }
                        }
                    }

                    // Mark remaining sensors as not found (disconnected) - requires debouncing too
                    for sensor_idx in devices.len()..config.num_lines as usize {
                        consecutive_failures[sensor_idx] += 1;
                        if consecutive_failures[sensor_idx] >= failure_debounce_count {
                            alarm_controllers[sensor_idx].mark_read_failure();
                            // Mark as disconnected in shared sensor state
                            let alarm_state = alarm_controllers[sensor_idx].state();
                            if let Ok(mut state) = sensor_state.write() {
                                state.set_reading(sensor_idx as u8, SensorReading::new(0.0, false, alarm_state));
                            }

                            // Publish disconnection to MQTT if available
                            if let Some(ref mqtt) = mqtt_handle {
                                mqtt.send_sensor_reading(sensor_idx as u8, 0.0, false, alarm_state);
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[SensorMonitor] Error enumerating W1 devices: {}", e);
                    // On enumeration error, track failure for all sensors with debouncing
                    for idx in 0..config.num_lines as usize {
                        consecutive_failures[idx] += 1;
                        if consecutive_failures[idx] >= failure_debounce_count {
                            alarm_controllers[idx].mark_read_failure();
                        }
                    }
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

            // Sleep before next update
            thread::sleep(update_interval);
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
