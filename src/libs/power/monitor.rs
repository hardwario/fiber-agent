// Background monitoring thread for continuous power monitoring

use std::io;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::drivers::stm::StmBridge;
use crate::libs::leds::SharedLedStateHandle;
use crate::libs::buzzer::{BuzzerController, BuzzerPriorityManager};
use crate::libs::buzzer::pattern::BuzzerPattern;
use crate::libs::config::BuzzerTiming;
use crate::libs::logging::get_timestamp_str;
use crate::libs::mqtt::messages::MqttMessage;
use crossbeam::channel::Sender;
use super::controller::PowerController;
use super::status::SharedPowerStatus;

/// Background power monitoring thread
pub struct PowerMonitor {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
}

impl PowerMonitor {
    /// Create and spawn background power monitoring thread
    ///
    /// The thread will continuously monitor power status and update the shared LED state
    /// at the specified update interval. Actual LED control happens in the dedicated LedMonitor thread.
    /// Also manages buzzer alerts for power events.
    pub fn new(
        stm: Arc<Mutex<StmBridge>>,
        update_interval_ms: u64,
        led_state: SharedLedStateHandle,
        buzzer: Arc<Mutex<BuzzerController>>,
        priority_manager: Arc<BuzzerPriorityManager>,
        power_status: SharedPowerStatus,
        mqtt_sender: Option<Sender<MqttMessage>>,
    ) -> io::Result<Self> {
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();

        let thread_handle = thread::spawn(move || {
            Self::monitor_loop(stm, shutdown_flag_clone, update_interval_ms, led_state, buzzer, priority_manager, power_status, mqtt_sender);
        });

        Ok(Self {
            thread_handle: Some(thread_handle),
            shutdown_flag,
        })
    }

    /// Background monitoring loop
    fn monitor_loop(
        stm: Arc<Mutex<StmBridge>>,
        shutdown_flag: Arc<AtomicBool>,
        update_interval_ms: u64,
        led_state: SharedLedStateHandle,
        buzzer: Arc<Mutex<BuzzerController>>,
        priority_manager: Arc<BuzzerPriorityManager>,
        power_status: SharedPowerStatus,
        mqtt_sender: Option<Sender<MqttMessage>>,
    ) {
        // Create power controller
        let mut controller = match PowerController::new(stm) {
            Ok(ctrl) => ctrl,
            Err(e) => {
                eprintln!("[PowerMonitor] Failed to initialize controller: {}", e);
                return;
            }
        };

        // Set update interval from configuration
        let update_interval = Duration::from_millis(update_interval_ms);

        // State tracking for buzzer alerts
        let mut previous_vin_status = false;  // Was on AC power?
        let mut previous_critical_status = false;  // Was battery critical?
        let mut last_battery_beep = Instant::now();  // When was the last battery mode beep?
        let battery_beep_interval = Duration::from_secs(10);  // Beep every 10 seconds in battery mode

        eprintln!("[{}] [PowerMonitor] Started power monitoring with {}ms interval", get_timestamp_str(), update_interval_ms);

        // Main monitoring loop
        loop {
            // Check for shutdown signal
            if shutdown_flag.load(Ordering::Relaxed) {
                eprintln!("[{}] [PowerMonitor] Shutdown signal received, exiting monitor thread", get_timestamp_str());
                break;
            }

            // Perform power status update
            //eprintln!("[{}] [PowerMonitor] Attempting ADC read...", get_timestamp_str());
            let update_start = Instant::now();
            match controller.update() {
                Ok(()) => {
                    let update_duration = update_start.elapsed();
                   // eprintln!("[{}] [PowerMonitor] ADC read completed in {}ms", get_timestamp_str(), update_duration.as_millis());
                    let status = controller.get_status();
                    let current_vin_status = status.is_on_dc_power();

/*                     eprintln!(
                        "[{}] [PowerMonitor] Battery: {} mV, VIN: {} mV, AC: {}, Low: {}, Critical: {}",
                        get_timestamp_str(),
                        status.vbat_mv,
                        status.vin_mv,
                        status.is_on_dc_power(),
                        status.is_low(),
                        status.is_critical()
                    ); */

                    // Update shared power status
                    if let Ok(mut ps) = power_status.lock() {
                        *ps = status;
                    }

                    // Update shared LED state
                    // The set_power_leds() method automatically notifies the LED monitor of changes
                    let (color, blink) = status.get_pwr_led_state();
                    led_state.set_power_leds(color, blink);

                    // Handle VIN connection/disconnection transitions
                    if current_vin_status && !previous_vin_status {
                        // VIN just connected (DC power detected)
                        eprintln!("[{}] [PowerMonitor] DC power detected - VIN connected", get_timestamp_str());

                        if let Ok(bz) = buzzer.lock() {
                            bz.play_once(BuzzerPattern::ReconnectionHappy { frequency_hz: 150 });
                        }

                        // Send power restored alarm event (clears active alarm)
                        if let Some(ref sender) = mqtt_sender {
                            let _ = sender.try_send(MqttMessage::PublishSystemAlarmEvent {
                                alarm_type: "POWER_DISCONNECT".to_string(),
                                name: "Power Supply".to_string(),
                                from_state: "CRITICAL".to_string(),
                                to_state: "NORMAL".to_string(),
                                message: "DC power restored".to_string(),
                            });
                        }
                    } else if !current_vin_status && previous_vin_status {
                        // VIN just disconnected (lost AC power, switched to battery)
                        eprintln!("[{}] [PowerMonitor] DC power lost - switched to battery", get_timestamp_str());

                        // Record DC loss timestamp in shared power status
                        if let Ok(mut ps) = power_status.lock() {
                            ps.record_dc_loss();
                            eprintln!("[{}] [PowerMonitor] DC loss timestamp recorded", get_timestamp_str());
                        }

                        if let Ok(bz) = buzzer.lock() {
                            let vin_disconnect_timing = BuzzerTiming {
                                on_ms: 2000,   // 2 second long beep
                                off_ms: 0,
                            };
                            bz.play_once(BuzzerPattern::VinDisconnectBeep(vin_disconnect_timing));
                        }

                        // Send power disconnect alarm event
                        if let Some(ref sender) = mqtt_sender {
                            let _ = sender.try_send(MqttMessage::PublishSystemAlarmEvent {
                                alarm_type: "POWER_DISCONNECT".to_string(),
                                name: "Power Supply".to_string(),
                                from_state: "NORMAL".to_string(),
                                to_state: "CRITICAL".to_string(),
                                message: "DC power disconnected".to_string(),
                            });
                        }
                    }

                    // Handle battery mode reminder beeps (every 10 seconds)
                    if status.is_on_battery() && last_battery_beep.elapsed() >= battery_beep_interval {
                        eprintln!("[{}] [PowerMonitor] Battery mode reminder beep", get_timestamp_str());
                        if let Ok(bz) = buzzer.lock() {
                            let battery_mode_timing = BuzzerTiming {
                                on_ms: 100,    // 100ms beep
                                off_ms: 100,
                            };
                            bz.play_once(BuzzerPattern::BatteryModeBeep(battery_mode_timing));
                        }
                        last_battery_beep = Instant::now();
                    }

                    // Handle critical battery alert (repeating)
                    // Use BuzzerPriorityManager to coordinate with sensor critical alarms
                    let current_critical_status = status.is_critical();
                    if current_critical_status && !previous_critical_status {
                        // Just entered critical state - notify priority manager
                        eprintln!("[{}] [PowerMonitor] Battery critical - notifying priority manager", get_timestamp_str());
                        priority_manager.set_battery_critical(true);
                    } else if !current_critical_status && previous_critical_status {
                        // Just left critical state - notify priority manager
                        eprintln!("[{}] [PowerMonitor] Battery recovered - clearing critical flag", get_timestamp_str());
                        priority_manager.set_battery_critical(false);
                    }

                    // Update previous states for next iteration
                    previous_vin_status = current_vin_status;
                    previous_critical_status = current_critical_status;
                }
                Err(e) => {
                    eprintln!("[{}] [PowerMonitor] Error during update: {}", get_timestamp_str(), e);
                    // Continue on error - don't crash the monitor thread
                }
            }

            // Sleep before next update
            thread::sleep(update_interval);
        }

        eprintln!("[{}] [PowerMonitor] Monitor thread exited cleanly", get_timestamp_str());
    }

    /// Gracefully shutdown the monitoring thread
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

impl Drop for PowerMonitor {
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
