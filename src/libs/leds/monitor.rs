// Dedicated LED control thread - manages all LED blinking with consistent timing

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::drivers::StmBridge;
use super::state::SharedLedStateHandle;

/// Dedicated LED control thread
///
/// This thread manages all LED updates (both power and line LEDs) at a fixed 50ms
/// interval, ensuring consistent blinking patterns independent of other monitoring
/// loops. This solves timing issues where LED updates were tied to sensor read
/// intervals (which have variable latency).
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
    /// Event-driven: waits for LED state changes and sends commands to STM32 immediately.
    /// Wakes up every 50ms to recalculate blink states, but only sends commands on actual change.
    fn monitor_loop(
        stm: Arc<Mutex<StmBridge>>,
        shutdown_flag: Arc<AtomicBool>,
        shared_state: SharedLedStateHandle,
    ) {
        const CHECK_INTERVAL_MS: u64 = 50;  // Check for state changes every 50ms to handle blink calculations
        let check_interval = Duration::from_millis(CHECK_INTERVAL_MS);

        eprintln!("[LedMonitor] Started (event-driven, checking every {}ms)", CHECK_INTERVAL_MS);

        // State caching: track last sent LED states to only send commands on change
        let mut last_line_states: [(bool, bool); 8] = [(false, false); 8];  // (green, red) for each line
        let mut last_power_leds: (bool, bool) = (false, false);  // (green, yellow)

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

            // Calculate blink cycle (0-7) based on elapsed time
            let blink_cycle = self::calculate_blink_cycle();

            // Calculate LED states for all configured lines
            let mut led_updates = Vec::new();
            for (idx, line_opt) in led_state.lines.iter().enumerate() {
                if let Some(line_state) = line_opt {
                    let (green, red) = self::get_led_pins_with_timing(
                        line_state.led_state,
                        blink_cycle,
                    );
                    led_updates.push((idx as u8, green, red));
                }
            }

            // Calculate power LED state
            let (power_green, power_yellow) = if led_state.power.blink {
                // LED should blink: calculate state based on elapsed time
                // Blinks at 1Hz: 500ms on, 500ms off
                let blink_on = (Instant::now().elapsed().as_millis() as u64 % 1000) < 500;
                let (g, y) = led_state.power.get_pins();
                if blink_on {
                    (g, y)
                } else {
                    (false, false)
                }
            } else {
                // LED should be steady
                led_state.power.get_pins()
            };

            // Acquire lock only to send commands that actually changed
            if let Ok(mut stm_guard) = stm.lock() {
                // Update line LEDs - only send commands if state changed
                for (idx, green, red) in led_updates {
                    let idx_usize = idx as usize;
                    if (green, red) != last_line_states[idx_usize] {
                        if let Err(e) = stm_guard.set_line_leds(idx, green, red) {
                            eprintln!("[LedMonitor] Error setting line LED {}: {}", idx, e);
                        } else {
                            last_line_states[idx_usize] = (green, red);
                        }
                    }
                }

                // Update power LEDs - only send commands if state changed
                if (power_green, power_yellow) != last_power_leds {
                    if let Err(e) = stm_guard.set_pwr_leds(power_green, power_yellow) {
                        eprintln!("[LedMonitor] Error setting power LED: {}", e);
                    } else {
                        last_power_leds = (power_green, power_yellow);
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

/// Calculate blink cycle (0-7) based on elapsed time since program start
/// With 50ms updates: 8 * 50ms = 400ms per full cycle
fn calculate_blink_cycle() -> u8 {
    // Use elapsed time from program start (this is stable across all threads)
    // Since we can't easily get a global start time, we'll derive it from system time
    // This gives us a blink cycle that's synchronized across the application
    let elapsed_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // 8-cycle * 50ms = 400ms per full blink cycle
    ((elapsed_ms / 50) % 8) as u8
}

/// Get LED pin states based on LED state and time-based blinking
/// This replaces the inline logic from sensor monitor
fn get_led_pins_with_timing(
    led_state: crate::libs::alarms::color::LedState,
    blink_cycle: u8,
) -> (bool, bool) {
    use crate::libs::alarms::color::{LedColor, BlinkPattern};

    let is_on = match led_state.pattern {
        BlinkPattern::Steady => true,
        BlinkPattern::BlinkSlow => blink_cycle < 4,      // 4 on, 4 off
        BlinkPattern::BlinkFast => blink_cycle % 2 == 0, // 1 on, 1 off
    };

    match (led_state.color, is_on) {
        (LedColor::Green, true) => (true, false),
        (LedColor::Red, true) => (false, true),
        (LedColor::Yellow, true) => (true, true),   // Both on for yellow (LEDG + LEDR)
        (LedColor::Off, true) => (false, false),
        (_, false) => (false, false),
    }
}
