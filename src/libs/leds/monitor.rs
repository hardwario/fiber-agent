// Dedicated LED control thread - manages all LED updates via firmware-managed blinking

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::drivers::StmBridge;
use crate::libs::alarms::color::{BlinkPattern, LedColor};
use super::state::{PowerLedColor, SharedLedStateHandle};

/// Dedicated LED control thread
///
/// This thread manages all LED updates (both power and line LEDs) by sending
/// state changes to the STM32 firmware. The firmware handles all blink timing
/// internally via a timer interrupt, eliminating UART latency-induced drift.
pub struct LedMonitor {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
    pub shared_state: SharedLedStateHandle,
}

impl LedMonitor {
    /// Create and spawn dedicated LED monitoring thread
    ///
    /// The thread is event-driven: it waits for LED state changes and sends commands
    /// to the STM32 immediately when changes occur.
    pub fn new(stm: Arc<Mutex<StmBridge>>) -> io::Result<Self> {
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();
        let shared_state = Arc::new(super::state::SharedLedStateWithNotify::new());
        let shared_state_clone = shared_state.clone();

        let thread_handle = thread::spawn(move || {
            Self::monitor_loop(stm, shutdown_flag_clone, shared_state_clone);
        });

        Ok(Self {
            thread_handle: Some(thread_handle),
            shutdown_flag,
            shared_state,
        })
    }

    /// Background LED monitoring loop
    ///
    /// Event-driven: waits for LED state changes and sends commands to STM32 when changes occur.
    /// Firmware handles all blink timing internally via TIM14 interrupt.
    fn monitor_loop(
        stm: Arc<Mutex<StmBridge>>,
        shutdown_flag: Arc<AtomicBool>,
        shared_state: SharedLedStateHandle,
    ) {
        const CHECK_INTERVAL_MS: u64 = 100;  // Only need to check for state changes
        let check_interval = Duration::from_millis(CHECK_INTERVAL_MS);

        eprintln!("[LedMonitor] Started (firmware-managed blinking)");

        // Track last sent states to only send on actual change
        // (color, pattern) for each line LED
        let mut last_line_states: [Option<(LedColor, BlinkPattern)>; 8] = [None; 8];
        // (color, blink) for power LED
        let mut last_power_state: Option<(PowerLedColor, bool)> = None;

        // Sync blink phase on startup
        if let Ok(mut stm_guard) = stm.lock() {
            if let Err(e) = stm_guard.sync_blink() {
                eprintln!("[LedMonitor] Warning: failed to sync blink phase: {}", e);
            }
        }

        // Main LED control loop
        loop {
            // Check for shutdown signal
            if shutdown_flag.load(Ordering::Relaxed) {
                eprintln!("[LedMonitor] Shutdown signal received, exiting monitor thread");
                break;
            }

            // Wait for state change notification or timeout
            shared_state.wait_for_change(check_interval);

            // Read current shared LED state
            let led_state = shared_state.read();

            // Acquire lock to send commands
            if let Ok(mut stm_guard) = stm.lock() {
                // Update line LEDs - only send if state changed
                for (idx, line_opt) in led_state.lines.iter().enumerate() {
                    if let Some(line_state) = line_opt {
                        let current = (line_state.led_state.color, line_state.led_state.pattern);
                        if last_line_states[idx] != Some(current) {
                            let color = match current.0 {
                                LedColor::Off => 'O',
                                LedColor::Green => 'G',
                                LedColor::Red => 'R',
                                LedColor::Yellow => 'Y',
                            };
                            let pattern = match current.1 {
                                BlinkPattern::Steady => 'S',
                                BlinkPattern::BlinkSlow => 'L',
                                BlinkPattern::BlinkFast => 'F',
                            };
                            eprintln!("[LedMonitor] DEBUG: Setting LED {} to color={} pattern={}", idx, color, pattern);
                            if let Err(e) = stm_guard.set_led_state(idx as u8, color, pattern) {
                                eprintln!("[LedMonitor] Error setting LED {}: {}", idx, e);
                            } else {
                                eprintln!("[LedMonitor] DEBUG: LED {} set successfully", idx);
                                last_line_states[idx] = Some(current);
                            }
                        }
                    }
                }

                // Update power LED - only send if state changed
                let power_current = (led_state.power.color, led_state.power.blink);
                if last_power_state != Some(power_current) {
                    let color = match led_state.power.color {
                        PowerLedColor::Off => 'O',
                        PowerLedColor::Green => 'G',
                        PowerLedColor::Yellow => 'Y',
                        PowerLedColor::Lime => 'L',
                    };
                    // Power LED blink uses slow pattern
                    let pattern = if led_state.power.blink { 'L' } else { 'S' };
                    eprintln!("[LedMonitor] DEBUG: Setting PWR LED to color={} pattern={}", color, pattern);
                    if let Err(e) = stm_guard.set_pwr_led_state(color, pattern) {
                        eprintln!("[LedMonitor] Error setting power LED: {}", e);
                    } else {
                        eprintln!("[LedMonitor] DEBUG: PWR LED set successfully");
                        last_power_state = Some(power_current);
                    }
                }
            } else {
                eprintln!("[LedMonitor] Failed to acquire STM lock for LED updates");
            }
        }

        eprintln!("[LedMonitor] Monitor thread exited cleanly");
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

impl Drop for LedMonitor {
    fn drop(&mut self) {
        // Signal shutdown on drop
        self.shutdown_flag.store(true, Ordering::Relaxed);

        // Wait for thread with timeout
        if let Some(handle) = self.thread_handle.take() {
            let timeout = Duration::from_secs(2);
            let start = std::time::Instant::now();
            while !handle.is_finished() && start.elapsed() < timeout {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
