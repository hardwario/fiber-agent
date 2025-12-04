//! Button monitoring thread for screen navigation control

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::drivers::buttons::{Buttons, ButtonEvent, Button};
use super::SharedDisplayStateHandle;

/// Button monitor state machine for handling ENTER button countdown
#[derive(Debug, Clone, Copy, PartialEq)]
enum ButtonMonitorState {
    /// Normal operation - waiting for button input
    Idle,
    /// ENTER button pressed, counting down for 5 seconds
    CountdownActive,
    /// QR code screen is displayed
    ShowingQr,
}

/// Button monitor thread for controlling display navigation
pub struct ButtonMonitor {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
}

impl ButtonMonitor {
    /// Create and spawn background button monitoring thread
    ///
    /// The thread will continuously monitor button input and update the display page
    /// when UP/DOWN buttons are pressed. The ENTER button is reserved for future use.
    pub fn new(
        display_state: SharedDisplayStateHandle,
    ) -> io::Result<Self> {
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();

        let thread_handle = thread::spawn(move || {
            Self::button_loop(shutdown_flag_clone, display_state);
        });

        Ok(Self {
            thread_handle: Some(thread_handle),
            shutdown_flag,
        })
    }

    /// Background button monitoring loop
    fn button_loop(
        shutdown_flag: Arc<AtomicBool>,
        display_state: SharedDisplayStateHandle,
    ) {
        // Initialize buttons
        let mut buttons = match Buttons::new() {
            Ok(btn) => {
                eprintln!("[ButtonMonitor] Buttons initialized successfully");
                btn
            }
            Err(e) => {
                eprintln!("[ButtonMonitor] Failed to initialize buttons: {}", e);
                return;
            }
        };

        eprintln!("[ButtonMonitor] Started button monitoring with 50ms poll interval");

        let poll_interval = Duration::from_millis(50);
        const COUNTDOWN_DURATION: Duration = Duration::from_secs(5);

        // Button monitor state machine
        let mut state = ButtonMonitorState::Idle;
        let mut countdown_start = Instant::now();

        // Main button monitoring loop
        loop {
            // Check for shutdown signal
            if shutdown_flag.load(Ordering::Relaxed) {
                eprintln!("[ButtonMonitor] Shutdown signal received, exiting button thread");
                break;
            }

            // Poll buttons for events
            let events = buttons.poll();

            // Process button events
            for event in events {
                match event {
                    ButtonEvent::Press(Button::Up) => {
                        eprintln!("[ButtonMonitor] UP button pressed");
                        // Navigate to next page (only if not showing QR code)
                        if let Ok(mut display_state_lock) = display_state.lock() {
                            if !display_state_lock.current_screen.is_qr_code() {
                                display_state_lock.next_page();
                                eprintln!("[ButtonMonitor] Page changed");
                            }
                        }
                        // Reset countdown state on any button press
                        state = ButtonMonitorState::Idle;
                    }
                    ButtonEvent::Press(Button::Down) => {
                        eprintln!("[ButtonMonitor] DOWN button pressed");
                        // Navigate to next page (only if not showing QR code)
                        if let Ok(mut display_state_lock) = display_state.lock() {
                            if !display_state_lock.current_screen.is_qr_code() {
                                display_state_lock.next_page();
                                eprintln!("[ButtonMonitor] Page changed");
                            }
                        }
                        // Reset countdown state on any button press
                        state = ButtonMonitorState::Idle;
                    }
                    ButtonEvent::Press(Button::Enter) => {
                        eprintln!("[ButtonMonitor] ENTER button pressed");
                        match state {
                            ButtonMonitorState::Idle => {
                                // Start countdown
                                state = ButtonMonitorState::CountdownActive;
                                countdown_start = Instant::now();
                                eprintln!("[ButtonMonitor] Starting 5-second countdown to QR code screen");
                            }
                            ButtonMonitorState::CountdownActive => {
                                // ENTER pressed again during countdown - restart
                                countdown_start = Instant::now();
                                eprintln!("[ButtonMonitor] Countdown restarted");
                            }
                            ButtonMonitorState::ShowingQr => {
                                // Return to sensor overview
                                if let Ok(mut display_state_lock) = display_state.lock() {
                                    display_state_lock.show_sensor_overview();
                                    eprintln!("[ButtonMonitor] Returning to sensor overview");
                                }
                                state = ButtonMonitorState::Idle;
                            }
                        }
                    }
                    ButtonEvent::Release(_) => {
                        // Ignore release events
                    }
                }
            }

            // Check countdown completion
            if state == ButtonMonitorState::CountdownActive {
                if countdown_start.elapsed() >= COUNTDOWN_DURATION {
                    // Transition to QR code screen
                    if let Ok(mut display_state_lock) = display_state.lock() {
                        display_state_lock.show_qr_code();
                        eprintln!("[ButtonMonitor] Countdown complete - transitioning to QR code screen");
                    }
                    state = ButtonMonitorState::ShowingQr;
                }
            }

            // Sleep before next poll
            thread::sleep(poll_interval);
        }

        eprintln!("[ButtonMonitor] Button monitor thread exited cleanly");
    }
}

impl Drop for ButtonMonitor {
    fn drop(&mut self) {
        // Signal shutdown on drop
        self.shutdown_flag.store(true, Ordering::Relaxed);

        // Wait for thread to finish with timeout
        if let Some(handle) = self.thread_handle.take() {
            let timeout = Duration::from_secs(2);
            let start = std::time::Instant::now();
            while !handle.is_finished() && start.elapsed() < timeout {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
