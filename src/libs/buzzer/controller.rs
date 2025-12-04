//! Public API for controlling buzzer patterns

use rppal::gpio::Gpio;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use super::pattern::{BuzzerPattern, SharedBuzzerState};
use super::thread::spawn_buzzer_thread;

/// Controller for managing buzzer patterns and thread
/// Provides a clean, thread-safe API for pattern management
pub struct BuzzerController {
    shutdown_flag: Arc<AtomicBool>,
    buzzer_state: Arc<SharedBuzzerState>,
    buzzer_thread_handle: Option<JoinHandle<()>>,
}

impl BuzzerController {
    /// Create and initialize a new buzzer controller
    /// Spawns the dedicated buzzer control thread
    pub fn new(gpio: Arc<Gpio>) -> io::Result<Self> {
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let buzzer_state = Arc::new(SharedBuzzerState::new());

        let shutdown_flag_clone = shutdown_flag.clone();
        let buzzer_state_clone = buzzer_state.clone();
        let gpio_clone = gpio.clone();

        let buzzer_thread_handle = spawn_buzzer_thread(shutdown_flag_clone, buzzer_state_clone, gpio_clone);

        Ok(Self {
            shutdown_flag,
            buzzer_state,
            buzzer_thread_handle: Some(buzzer_thread_handle),
        })
    }

    /// Set a repeating pattern that continues until changed or stopped
    /// Used for continuous alarm patterns: Disconnected, Critical
    pub fn set_repeating_pattern(&self, pattern: BuzzerPattern) {
        eprintln!("[BuzzerController] Set repeating pattern: {:?}", pattern);
        self.buzzer_state.set_pattern(pattern);
    }

    /// Play a one-time pattern that automatically stops after duration
    /// Used for notification patterns: VinDisconnectBeep, BatteryModeBeep, ReconnectionHappy
    pub fn play_once(&self, pattern: BuzzerPattern) {
        eprintln!("[BuzzerController] Play one-time pattern: {:?}", pattern);
        self.buzzer_state.set_pattern(pattern);
    }

    /// Immediately stop any buzzer pattern
    pub fn stop(&self) {
        eprintln!("[BuzzerController] Buzzer stopped");
        self.buzzer_state.set_pattern(BuzzerPattern::Off);
    }

    /// Gracefully shutdown the controller and buzzer thread
    pub fn shutdown(mut self) -> io::Result<()> {
        // Signal the buzzer thread to shutdown
        self.shutdown_flag.store(true, Ordering::Relaxed);

        // Wait for buzzer thread to finish
        if let Some(handle) = self.buzzer_thread_handle.take() {
            handle.join().ok();
        }

        eprintln!("[BuzzerController] Shutdown complete");
        Ok(())
    }
}

impl Drop for BuzzerController {
    fn drop(&mut self) {
        // Signal shutdown on drop
        self.shutdown_flag.store(true, Ordering::Relaxed);

        // Wait for buzzer thread with a timeout
        if let Some(handle) = self.buzzer_thread_handle.take() {
            let timeout = std::time::Duration::from_millis(100);
            let start = std::time::Instant::now();
            while !handle.is_finished() && start.elapsed() < timeout {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}
