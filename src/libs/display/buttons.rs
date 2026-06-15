//! Button monitoring thread for screen navigation control

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::drivers::buttons::{Buttons, ButtonEvent, Button};
use crate::libs::network::{ProvisioningSession, SharedProvisioningSession};
use crate::libs::pairing::{PairingHandle, SharedPairingStateHandle};
use crate::libs::buzzer::BuzzerPriorityManager;
use super::SharedDisplayStateHandle;

/// Button monitor state machine for handling ENTER button countdown and DOWN button hold
#[derive(Debug, Clone, Copy, PartialEq)]
enum ButtonMonitorState {
    /// Normal operation - waiting for button input
    Idle,
    /// ENTER button pressed, counting down for 2 seconds
    CountdownActive,
    /// QR code screen is displayed
    ShowingQr,
    /// DOWN button being held, counting down for 2 seconds
    DownHoldActive,
    /// System info screen is displayed
    ShowingSystem,
    /// UP button being held, counting down for 2 seconds for pairing
    UpHoldActive,
    /// Pairing screen is displayed
    ShowingPairing,
    /// Sensor selection mode - cursor navigation active
    SelectionMode,
    /// Viewing sensor detail screen
    ShowingDetail,
}

/// Map elapsed/total to a 1-pixel-tall progress bar width in the 0..=127 range.
/// Saturates at 127 once the hold completes.
fn progress_pixels(elapsed: Duration, total: Duration) -> u8 {
    if total.is_zero() {
        return 0;
    }
    let ratio = (elapsed.as_millis() as f64 / total.as_millis() as f64).clamp(0.0, 1.0);
    (ratio * 127.0).round() as u8
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
    /// If a pairing_handle is provided, UP held for 2 seconds triggers pairing mode.
    pub fn new(
        display_state: SharedDisplayStateHandle,
        pairing_handle: Option<PairingHandle>,
        buzzer_priority: Option<Arc<BuzzerPriorityManager>>,
        pairing_state: Option<SharedPairingStateHandle>,
        sensor_state: crate::libs::sensors::SharedSensorStateHandle,
        provisioning_session: SharedProvisioningSession,
        mac_address: String,
        hostname: String,
    ) -> io::Result<Self> {
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();

        let sensor_state_clone = sensor_state.clone();
        let thread_handle = thread::spawn(move || {
            Self::button_loop(
                shutdown_flag_clone,
                display_state,
                pairing_handle,
                buzzer_priority,
                pairing_state,
                sensor_state_clone,
                provisioning_session,
                mac_address,
                hostname,
            );
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
        pairing_handle: Option<PairingHandle>,
        buzzer_priority: Option<Arc<BuzzerPriorityManager>>,
        pairing_state: Option<SharedPairingStateHandle>,
        sensor_state: crate::libs::sensors::SharedSensorStateHandle,
        provisioning_session: SharedProvisioningSession,
        mac_address: String,
        hostname: String,
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
        const COUNTDOWN_DURATION: Duration = Duration::from_millis(2000);
        const SELECTION_TIMEOUT: Duration = Duration::from_secs(15);
        const DOUBLE_CLICK_THRESHOLD: Duration = Duration::from_millis(400);

        // Button monitor state machine
        let mut state = ButtonMonitorState::Idle;
        let mut countdown_start = Instant::now();
        let mut down_hold_start = Instant::now();
        let mut up_hold_start = Instant::now();
        let mut selection_activity = Instant::now(); // Track last activity in selection/detail mode
        let mut last_enter_click: Option<Instant> = None; // Track last ENTER click for double-click detection

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
                // Any button PRESS silences sensor beep (consumes the event)
                if let ButtonEvent::Press(_) = &event {
                    if let Some(ref bp) = buzzer_priority {
                        if bp.is_sensor_beeping() {
                            bp.silence_sensor_30min();
                            eprintln!("[ButtonMonitor] Sensor beep silenced by button press (30 min)");
                            continue;
                        }
                    }
                }

                match event {
                    ButtonEvent::Press(Button::Up) => {
                        eprintln!("[ButtonMonitor] UP button pressed");

                        // Handle ShowingPairing - any button cancels
                        if state == ButtonMonitorState::ShowingPairing {
                            if let Some(ref ph) = pairing_handle {
                                ph.cancel_pairing();
                            }
                            if let Ok(mut display_state_lock) = display_state.lock() {
                                display_state_lock.show_sensor_overview();
                            }
                            state = ButtonMonitorState::Idle;
                            eprintln!("[ButtonMonitor] Pairing cancelled by UP button");
                            continue;
                        }

                        match state {
                            ButtonMonitorState::Idle => {
                                // Check if on special screen (QR code only)
                                let on_special_screen = if let Ok(display_state_lock) = display_state.lock() {
                                    display_state_lock.current_screen.is_special_screen()
                                } else {
                                    false
                                };

                                if on_special_screen {
                                    // On QR screen - ignore UP
                                    eprintln!("[ButtonMonitor] UP ignored on QR code screen");
                                } else {
                                    // Start UP hold countdown (for pairing if handle available)
                                    state = ButtonMonitorState::UpHoldActive;
                                    up_hold_start = Instant::now();
                                    eprintln!("[ButtonMonitor] UP hold started - counting 2 seconds for pairing");
                                }
                            }
                            ButtonMonitorState::ShowingSystem => {
                                // On System screen - navigate pages instead of starting hold
                                if let Ok(mut display_state_lock) = display_state.lock() {
                                    display_state_lock.next_page();
                                    eprintln!("[ButtonMonitor] System info page changed");
                                }
                            }
                            ButtonMonitorState::SelectionMode => {
                                // In selection mode - move cursor up
                                let ds_readings = sensor_state.read().map(|s| s.readings.clone()).unwrap_or_else(|_| [None, None, None, None, None, None, None, None]);
                                if let Ok(mut display_state_lock) = display_state.lock() {
                                    display_state_lock.selection_up(&ds_readings);
                                    eprintln!("[ButtonMonitor] Selection cursor moved up");
                                }
                                selection_activity = Instant::now(); // Reset inactivity timer
                            }
                            ButtonMonitorState::UpHoldActive => {
                                // Already holding UP - ignore additional presses
                            }
                            ButtonMonitorState::CountdownActive => {
                                // Cancel ENTER countdown and start UP hold
                                state = ButtonMonitorState::UpHoldActive;
                                up_hold_start = Instant::now();
                                eprintln!("[ButtonMonitor] ENTER countdown cancelled, UP hold started");
                            }
                            ButtonMonitorState::DownHoldActive => {
                                // Cancel DOWN hold
                                state = ButtonMonitorState::Idle;
                                eprintln!("[ButtonMonitor] DOWN hold cancelled by UP button");
                            }
                            ButtonMonitorState::ShowingDetail => {
                                if let Ok(mut display_state_lock) = display_state.lock() {
                                    display_state_lock.lorawan_detail_prev();
                                }
                            }
                            _ => {
                                // Other states (ShowingQr) - ignore UP
                            }
                        }
                    }
                    ButtonEvent::Release(Button::Up) => {
                        if state == ButtonMonitorState::UpHoldActive {
                            let elapsed = up_hold_start.elapsed();
                            if elapsed < COUNTDOWN_DURATION {
                                // Released early - cancel hold and navigate page
                                eprintln!("[ButtonMonitor] UP released early ({:.1}s) - navigating page", elapsed.as_secs_f32());
                                if let Ok(mut display_state_lock) = display_state.lock() {
                                    if display_state_lock.current_screen.is_navigable() {
                                        display_state_lock.next_page();
                                        eprintln!("[ButtonMonitor] Page changed");
                                    }
                                }
                                state = ButtonMonitorState::Idle;
                            }
                            // If >= 2 seconds, already transitioned to ShowingPairing
                        }
                    }
                    ButtonEvent::Press(Button::Down) => {
                        eprintln!("[ButtonMonitor] DOWN button pressed");

                        // Handle ShowingPairing - any button cancels
                        if state == ButtonMonitorState::ShowingPairing {
                            if let Some(ref ph) = pairing_handle {
                                ph.cancel_pairing();
                            }
                            if let Ok(mut display_state_lock) = display_state.lock() {
                                display_state_lock.show_sensor_overview();
                            }
                            state = ButtonMonitorState::Idle;
                            eprintln!("[ButtonMonitor] Pairing cancelled by DOWN button");
                            continue;
                        }

                        match state {
                            ButtonMonitorState::Idle => {
                                // Check if on special screen (QR code only)
                                let on_special_screen = if let Ok(display_state_lock) = display_state.lock() {
                                    display_state_lock.current_screen.is_special_screen()
                                } else {
                                    false
                                };

                                if on_special_screen {
                                    // On QR screen - ignore DOWN
                                    eprintln!("[ButtonMonitor] DOWN ignored on QR code screen");
                                } else {
                                    // Start DOWN hold countdown
                                    state = ButtonMonitorState::DownHoldActive;
                                    down_hold_start = Instant::now();
                                    eprintln!("[ButtonMonitor] DOWN hold started - counting 2 seconds");
                                }
                            }
                            ButtonMonitorState::ShowingSystem => {
                                // On System screen - navigate pages instead of starting hold
                                if let Ok(mut display_state_lock) = display_state.lock() {
                                    display_state_lock.next_page();
                                    eprintln!("[ButtonMonitor] System info page changed");
                                }
                            }
                            ButtonMonitorState::DownHoldActive => {
                                // Already holding DOWN - ignore additional presses
                            }
                            ButtonMonitorState::CountdownActive => {
                                // Cancel ENTER countdown and start DOWN hold
                                state = ButtonMonitorState::DownHoldActive;
                                down_hold_start = Instant::now();
                                eprintln!("[ButtonMonitor] ENTER countdown cancelled, DOWN hold started");
                            }
                            ButtonMonitorState::UpHoldActive => {
                                // Cancel UP hold
                                state = ButtonMonitorState::Idle;
                                eprintln!("[ButtonMonitor] UP hold cancelled by DOWN button");
                            }
                            ButtonMonitorState::SelectionMode => {
                                // In selection mode - move cursor down
                                let ds_readings = sensor_state.read().map(|s| s.readings.clone()).unwrap_or_else(|_| [None, None, None, None, None, None, None, None]);
                                if let Ok(mut display_state_lock) = display_state.lock() {
                                    display_state_lock.selection_down(&ds_readings);
                                    eprintln!("[ButtonMonitor] Selection cursor moved down");
                                }
                                selection_activity = Instant::now(); // Reset inactivity timer
                            }
                            ButtonMonitorState::ShowingDetail => {
                                if let Ok(mut display_state_lock) = display_state.lock() {
                                    display_state_lock.lorawan_detail_next();
                                }
                            }
                            _ => {
                                // Other states (ShowingQr) - ignore DOWN
                            }
                        }
                    }
                    ButtonEvent::Release(Button::Down) => {
                        if state == ButtonMonitorState::DownHoldActive {
                            let elapsed = down_hold_start.elapsed();
                            if elapsed < COUNTDOWN_DURATION {
                                // Released early - cancel hold and navigate page
                                eprintln!("[ButtonMonitor] DOWN released early ({:.1}s) - navigating page", elapsed.as_secs_f32());
                                if let Ok(mut display_state_lock) = display_state.lock() {
                                    display_state_lock.next_page();
                                    eprintln!("[ButtonMonitor] Page changed");
                                }
                                state = ButtonMonitorState::Idle;
                            }
                            // If >= 2 seconds, already transitioned to ShowingSystem
                        }
                    }
                    ButtonEvent::Press(Button::Enter) => {
                        eprintln!("[ButtonMonitor] ENTER button pressed");

                        // Handle ShowingPairing - any button cancels
                        if state == ButtonMonitorState::ShowingPairing {
                            if let Some(ref ph) = pairing_handle {
                                ph.cancel_pairing();
                            }
                            if let Ok(mut display_state_lock) = display_state.lock() {
                                display_state_lock.show_sensor_overview();
                            }
                            state = ButtonMonitorState::Idle;
                            eprintln!("[ButtonMonitor] Pairing cancelled by ENTER button");
                            continue;
                        }

                        match state {
                            ButtonMonitorState::Idle => {
                                // Start countdown
                                state = ButtonMonitorState::CountdownActive;
                                countdown_start = Instant::now();
                                eprintln!("[ButtonMonitor] Starting 2-second countdown to QR code screen");
                            }
                            ButtonMonitorState::CountdownActive => {
                                // ENTER pressed again during countdown - restart
                                countdown_start = Instant::now();
                                eprintln!("[ButtonMonitor] Countdown restarted");
                            }
                            ButtonMonitorState::ShowingQr => {
                                // End the provisioning session before tearing
                                // down BLE — invalidates the token first so a
                                // racing pairing attempt fails closed.
                                if let Ok(mut slot) = provisioning_session.write() {
                                    if slot.is_some() {
                                        eprintln!("[ButtonMonitor] Provisioning session closed by user");
                                    }
                                    *slot = None;
                                }
                                // Stop BLE advertising
                                if let Err(e) = crate::libs::ble::stop_ble_advertising() {
                                    eprintln!("[ButtonMonitor] Failed to stop BLE advertising: {}", e);
                                }
                                // Return to sensor overview
                                if let Ok(mut display_state_lock) = display_state.lock() {
                                    display_state_lock.show_sensor_overview();
                                    eprintln!("[ButtonMonitor] Returning to sensor overview");
                                }
                                state = ButtonMonitorState::Idle;
                            }
                            ButtonMonitorState::DownHoldActive => {
                                // Cancel DOWN hold and start ENTER countdown
                                state = ButtonMonitorState::CountdownActive;
                                countdown_start = Instant::now();
                                eprintln!("[ButtonMonitor] DOWN hold cancelled, ENTER countdown started");
                            }
                            ButtonMonitorState::ShowingSystem => {
                                // Return to sensor overview
                                if let Ok(mut display_state_lock) = display_state.lock() {
                                    display_state_lock.show_sensor_overview();
                                    eprintln!("[ButtonMonitor] Exiting system info screen");
                                }
                                state = ButtonMonitorState::Idle;
                            }
                            ButtonMonitorState::UpHoldActive => {
                                // Cancel UP hold (pairing countdown)
                                state = ButtonMonitorState::Idle;
                                eprintln!("[ButtonMonitor] UP hold cancelled by ENTER");
                            }
                            ButtonMonitorState::ShowingPairing => {
                                // Already handled above
                            }
                            ButtonMonitorState::SelectionMode | ButtonMonitorState::ShowingDetail => {
                                // Actions handled on release for double-click detection
                                selection_activity = Instant::now();
                            }
                        }
                    }
                    ButtonEvent::Release(Button::Enter) => {
                        match state {
                            ButtonMonitorState::CountdownActive => {
                                let elapsed = countdown_start.elapsed();
                                if elapsed < COUNTDOWN_DURATION {
                                    // Released early - check for double-click to enter selection mode
                                    let on_sensor_overview = if let Ok(display_state_lock) = display_state.lock() {
                                        display_state_lock.current_screen.is_sensor_overview()
                                    } else {
                                        false
                                    };

                                    if on_sensor_overview {
                                        // Check if this is a double-click
                                        let is_double_click = last_enter_click
                                            .map(|last| last.elapsed() < DOUBLE_CLICK_THRESHOLD)
                                            .unwrap_or(false);

                                        if is_double_click {
                                            // Double-click detected - enter selection mode
                                            let ds_readings = sensor_state.read().map(|s| s.readings.clone()).unwrap_or_else(|_| [None, None, None, None, None, None, None, None]);
                                            if let Ok(mut display_state_lock) = display_state.lock() {
                                                display_state_lock.enter_selection_mode(&ds_readings);
                                                eprintln!("[ButtonMonitor] Double-click detected - entering selection mode");
                                            }
                                            state = ButtonMonitorState::SelectionMode;
                                            selection_activity = Instant::now();
                                            last_enter_click = None;
                                        } else {
                                            // First click - record time and wait for second click
                                            last_enter_click = Some(Instant::now());
                                            eprintln!("[ButtonMonitor] ENTER click recorded - waiting for double-click");
                                            state = ButtonMonitorState::Idle;
                                        }
                                    } else {
                                        // Not on sensor overview - just cancel countdown
                                        eprintln!("[ButtonMonitor] ENTER released early ({:.1}s) - countdown cancelled", elapsed.as_secs_f32());
                                        state = ButtonMonitorState::Idle;
                                    }
                                }
                                // If >= 2 seconds, already transitioned to ShowingQr
                            }
                            ButtonMonitorState::SelectionMode => {
                                // In selection mode - check for double-click to exit
                                let is_double_click = last_enter_click
                                    .map(|last| last.elapsed() < DOUBLE_CLICK_THRESHOLD)
                                    .unwrap_or(false);

                                if is_double_click {
                                    // Double-click detected - exit selection mode
                                    if let Ok(mut display_state_lock) = display_state.lock() {
                                        display_state_lock.show_sensor_overview();
                                        eprintln!("[ButtonMonitor] Double-click detected - exiting selection mode");
                                    }
                                    state = ButtonMonitorState::Idle;
                                    last_enter_click = None;
                                } else {
                                    // Single click - enter detail view
                                    if let Ok(mut display_state_lock) = display_state.lock() {
                                        display_state_lock.enter_detail_view();
                                        eprintln!("[ButtonMonitor] Entering sensor detail view");
                                    }
                                    state = ButtonMonitorState::ShowingDetail;
                                    selection_activity = Instant::now();
                                    last_enter_click = Some(Instant::now());
                                }
                            }
                            ButtonMonitorState::ShowingDetail => {
                                // In detail view - check for double-click to exit completely
                                let is_double_click = last_enter_click
                                    .map(|last| last.elapsed() < DOUBLE_CLICK_THRESHOLD)
                                    .unwrap_or(false);

                                if is_double_click {
                                    // Double-click - exit to sensor overview
                                    if let Ok(mut display_state_lock) = display_state.lock() {
                                        display_state_lock.show_sensor_overview();
                                        eprintln!("[ButtonMonitor] Double-click detected - exiting to sensor overview");
                                    }
                                    state = ButtonMonitorState::Idle;
                                    last_enter_click = None;
                                } else {
                                    // Single click - exit to selection mode
                                    let ds_readings = sensor_state.read().map(|s| s.readings.clone()).unwrap_or_else(|_| [None, None, None, None, None, None, None, None]);
                                    if let Ok(mut display_state_lock) = display_state.lock() {
                                        display_state_lock.exit_detail_view(&ds_readings);
                                        eprintln!("[ButtonMonitor] Exiting sensor detail view to selection mode");
                                    }
                                    state = ButtonMonitorState::SelectionMode;
                                    selection_activity = Instant::now();
                                    last_enter_click = Some(Instant::now());
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }

            // Check countdown completion for ENTER button
            if state == ButtonMonitorState::CountdownActive {
                if countdown_start.elapsed() >= COUNTDOWN_DURATION {
                    // Mint a fresh ephemeral provisioning session BEFORE
                    // starting BLE advertising — the GATT auth path reads
                    // this on every pairing attempt, so it must be live by
                    // the time the phone connects.
                    match ProvisioningSession::new(&mac_address, &hostname) {
                        Ok(session) => {
                            if let Ok(mut slot) = provisioning_session.write() {
                                eprintln!(
                                    "[ButtonMonitor] Provisioning session opened (token={}, created_at={})",
                                    session.token(), session.created_at_unix(),
                                );
                                *slot = Some(session);
                            } else {
                                eprintln!("[ButtonMonitor] Failed to lock provisioning session for write");
                            }
                        }
                        Err(e) => {
                            eprintln!("[ButtonMonitor] Failed to mint provisioning session: {}", e);
                        }
                    }

                    // Start BLE advertising
                    if let Err(e) = crate::libs::ble::start_ble_advertising() {
                        eprintln!("[ButtonMonitor] Failed to start BLE advertising: {}", e);
                    }
                    // Transition to QR code screen
                    if let Ok(mut display_state_lock) = display_state.lock() {
                        display_state_lock.show_qr_code();
                        eprintln!("[ButtonMonitor] Countdown complete - transitioning to QR code screen");
                    }
                    state = ButtonMonitorState::ShowingQr;
                }
            }

            // Expire the provisioning session once it has sat idle for
            // IDLE_TIMEOUT (no BLE GATT activity from the phone). Active
            // sessions stay alive — each FB0x op bumps last_activity, so
            // a user mid-flow won't get kicked out. We clear the shared
            // slot, drop BLE advertising, and bounce the user back to the
            // sensor overview. The display will fall through to
            // render_qr_session_ended_screen for one frame if the user is
            // still on the QR screen between the expiry and the screen swap.
            if state == ButtonMonitorState::ShowingQr {
                let expired = provisioning_session
                    .read()
                    .ok()
                    .map(|g| g.as_ref().map(|s| s.is_expired()).unwrap_or(true))
                    .unwrap_or(false);
                if expired {
                    eprintln!("[ButtonMonitor] Provisioning session idle for 5min - tearing down");
                    if let Ok(mut slot) = provisioning_session.write() {
                        *slot = None;
                    }
                    if let Err(e) = crate::libs::ble::stop_ble_advertising() {
                        eprintln!("[ButtonMonitor] Failed to stop BLE advertising: {}", e);
                    }
                    if let Ok(mut display_state_lock) = display_state.lock() {
                        display_state_lock.show_sensor_overview();
                    }
                    state = ButtonMonitorState::Idle;
                }
            }

            // Check countdown completion for DOWN button hold
            if state == ButtonMonitorState::DownHoldActive {
                if down_hold_start.elapsed() >= COUNTDOWN_DURATION {
                    // Transition to system info screen
                    if let Ok(mut display_state_lock) = display_state.lock() {
                        display_state_lock.show_system_info();
                        eprintln!("[ButtonMonitor] DOWN hold complete (3s) - transitioning to system info screen");
                    }
                    state = ButtonMonitorState::ShowingSystem;
                }
            }

            // Check countdown completion for UP button hold (pairing)
            if state == ButtonMonitorState::UpHoldActive {
                if up_hold_start.elapsed() >= COUNTDOWN_DURATION {
                    // Check whether a BLE client is currently connected; if so, the
                    // MQTT pairing flow must not start (it would race with BLE for LCD).
                    let ble_is_active = if let Some(ref ps) = pairing_state {
                        let st = ps.lock().unwrap_or_else(|e| e.into_inner());
                        st.ble_active()
                    } else {
                        false
                    };

                    if ble_is_active {
                        eprintln!("[ButtonMonitor] UP+DOWN ignored: BLE client connected");
                        state = ButtonMonitorState::Idle;
                    } else if let Some(ref ph) = pairing_handle {
                        ph.start_pairing();
                        eprintln!("[ButtonMonitor] UP hold complete (3s) - triggering pairing mode");
                        state = ButtonMonitorState::ShowingPairing;
                    } else {
                        eprintln!("[ButtonMonitor] UP hold complete (3s) - pairing not available (MQTT disabled)");
                        state = ButtonMonitorState::Idle;
                    }
                }
            }

            // Check inactivity timeout for selection/detail mode (15 seconds)
            if (state == ButtonMonitorState::SelectionMode || state == ButtonMonitorState::ShowingDetail)
                && selection_activity.elapsed() >= SELECTION_TIMEOUT
            {
                // Return to normal sensor overview
                if let Ok(mut display_state_lock) = display_state.lock() {
                    display_state_lock.show_sensor_overview();
                    eprintln!("[ButtonMonitor] Selection mode timeout (15s) - returning to normal view");
                }
                state = ButtonMonitorState::Idle;
            }

            // Publish the current hold progress (0..=127) so the display monitor
            // can render a 1-px progress bar under the header divider while a
            // button is held. Cleared to 0 whenever no hold is in progress.
            let bar_pixels = match state {
                ButtonMonitorState::CountdownActive => {
                    progress_pixels(countdown_start.elapsed(), COUNTDOWN_DURATION)
                }
                ButtonMonitorState::DownHoldActive => {
                    progress_pixels(down_hold_start.elapsed(), COUNTDOWN_DURATION)
                }
                ButtonMonitorState::UpHoldActive => {
                    progress_pixels(up_hold_start.elapsed(), COUNTDOWN_DURATION)
                }
                _ => 0,
            };
            if let Ok(mut ds) = display_state.lock() {
                if ds.hold_bar_pixels != bar_pixels {
                    ds.hold_bar_pixels = bar_pixels;
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
