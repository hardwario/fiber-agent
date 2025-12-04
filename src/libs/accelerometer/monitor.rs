// Background monitoring thread for accelerometer motion detection

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use crate::drivers::lis2dh12::Lis2dh12;
use crate::libs::config::AccelerometerConfig;
use super::state::{MotionDetector, MotionState};

/// Background accelerometer monitoring thread
pub struct AccelerometerMonitor {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
}

impl AccelerometerMonitor {
    /// Create and spawn background accelerometer monitoring thread
    pub fn new(config: AccelerometerConfig) -> io::Result<Self> {
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();

        let thread_handle = thread::spawn(move || {
            Self::monitor_loop(config, shutdown_flag_clone);
        });

        Ok(Self {
            thread_handle: Some(thread_handle),
            shutdown_flag,
        })
    }

    /// Background monitoring loop
    fn monitor_loop(config: AccelerometerConfig, shutdown_flag: Arc<AtomicBool>) {
        // Open I2C device
        let mut accel = match Lis2dh12::new(&config.i2c_path) {
            Ok(a) => a,
            Err(e) => {
                eprintln!(
                    "[AccelerometerMonitor] Failed to initialize accelerometer at {}: {}",
                    config.i2c_path, e
                );
                return;
            }
        };

        // Create motion detector
        let mut detector = MotionDetector::new(config.motion_threshold_g, config.debounce_samples);
        let update_interval = Duration::from_millis(config.update_interval_ms);

        eprintln!(
            "[AccelerometerMonitor] Started monitoring at {} with {}ms interval, {}g threshold, {} debounce samples",
            config.i2c_path, config.update_interval_ms, config.motion_threshold_g, config.debounce_samples
        );

        // Main monitoring loop
        loop {
            // Check for shutdown signal
            if shutdown_flag.load(Ordering::Relaxed) {
                eprintln!("[AccelerometerMonitor] Shutdown signal received, exiting monitor thread");
                break;
            }

            // Read accelerometer data
            match accel.read() {
                Ok(accel_data) => {
                    let (new_state, state_changed) = detector.update(&accel_data);

                    if state_changed && config.logging_enabled {
                        let timestamp = SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .map(|d| format!("{:.3}s", d.as_secs_f64()))
                            .unwrap_or_else(|_| "unknown".to_string());

                        match new_state {
                            MotionState::Moving => {
                                let magnitude =
                                    (accel_data.x_g * accel_data.x_g
                                        + accel_data.y_g * accel_data.y_g
                                        + accel_data.z_g * accel_data.z_g)
                                        .sqrt();
                                eprintln!(
                                    "[AccelerometerMonitor] Motion detected at {} (magnitude: {:.2}g)",
                                    timestamp, magnitude
                                );
                            }
                            MotionState::Idle => {
                                eprintln!("[AccelerometerMonitor] Motion stopped at {}", timestamp);
                            }
                            MotionState::Debouncing { .. } => {
                                // Don't log debouncing state changes
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[AccelerometerMonitor] Error reading accelerometer: {}", e);
                    // Continue on error - don't crash the monitor thread
                }
            }

            // Sleep before next update
            thread::sleep(update_interval);
        }

        eprintln!("[AccelerometerMonitor] Monitor thread exited cleanly");
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

impl Drop for AccelerometerMonitor {
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
