//! Buzzer control thread - executes buzzer patterns independently

use crate::drivers::Buzzer;
use rppal::gpio::Gpio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::pattern::{BuzzerPattern, SharedBuzzerState};

/// Spawn and run the dedicated buzzer control thread
pub fn spawn_buzzer_thread(
    shutdown_flag: Arc<AtomicBool>,
    buzzer_state: Arc<SharedBuzzerState>,
    gpio: Arc<Gpio>,
) -> std::thread::JoinHandle<()> {
    thread::spawn(move || {
        buzzer_control_loop(shutdown_flag, buzzer_state, gpio);
    })
}

/// Dedicated buzzer control thread - runs independently
fn buzzer_control_loop(shutdown_flag: Arc<AtomicBool>, buzzer_state: Arc<SharedBuzzerState>, gpio: Arc<Gpio>) {
    // Initialize buzzer hardware
    let mut buzzer = match Buzzer::new(&gpio) {
        Ok(b) => {
            eprintln!("[BuzzerThread] Buzzer initialized successfully");
            Some(b)
        }
        Err(e) => {
            eprintln!("[BuzzerThread] Warning: Failed to initialize buzzer: {}", e);
            None
        }
    };

    let mut buzzer_is_on = false;

    loop {
        // Check for shutdown signal
        if shutdown_flag.load(Ordering::Relaxed) {
            //eprintln!("[BuzzerThread] Shutdown signal received, exiting buzzer thread");
            break;
        }

        // Wait for notification (wakes immediately on pattern change, or 50ms timeout during active pattern)
        buzzer_state.wait_for_event();

        // Read current buzzer state after wait completes
        let state_inner = buzzer_state.read();

        match &state_inner.pattern {
            BuzzerPattern::Off => {
                // Buzzer off - ensure pin is high
                if buzzer_is_on {
                    if let Some(ref mut bz) = buzzer {
                        bz.off();
                        //eprintln!("[BuzzerThread] >>> Buzzer OFF");
                    }
                    buzzer_is_on = false;
                }
            }
            _ => {
                // Beep or critical pattern - calculate phase
                let elapsed = state_inner.pattern_start_time.elapsed().as_millis() as u64;
                let should_beep = match &state_inner.pattern {
                    BuzzerPattern::ReconnectionHappy { frequency_hz } => {
                        // Happy beep pattern with PWM: 3 short beeps (200ms on, 150ms off each)
                        // Pattern plays for 1050ms total (3 beeps + pauses), then auto-stops
                        // Beeps at: 0-200ms, 350-550ms, 700-900ms, then silent until 1050ms
                        if elapsed >= 1050 {
                            // Happy beep duration expired - turn off
                            false
                        } else {
                            let phase = elapsed % 1050;
                            let is_beep_phase = (phase < 200) || (phase >= 350 && phase < 550) || (phase >= 700 && phase < 900);

                            // Simulate PWM by toggling rapidly within beep phase
                            if is_beep_phase {
                                let period_us = 1_000_000 / *frequency_hz as u64;
                                let pwm_cycle = (elapsed % period_us) < (period_us / 2);
                                pwm_cycle
                            } else {
                                false
                            }
                        }
                    }
                    BuzzerPattern::VinDisconnectBeep(timing) => {
                        // VinDisconnectBeep: single long beep (typically 2000ms on, 0ms off), then auto-stop
                        let total_duration = timing.on_ms + timing.off_ms;
                        if elapsed >= total_duration {
                            // Pattern duration expired - turn off
                            false
                        } else {
                            elapsed < timing.on_ms
                        }
                    }
                    BuzzerPattern::BatteryModeBeep(timing) => {
                        // BatteryModeBeep: single short beep (typically 100ms on, 100ms off), then auto-stop
                        let total_duration = timing.on_ms + timing.off_ms;
                        if elapsed >= total_duration {
                            // Pattern duration expired - turn off
                            false
                        } else {
                            elapsed < timing.on_ms
                        }
                    }
                    _ => {
                        // Standard repeating beep patterns: DisconnectedBeep, CriticalBeep
                        let (on_ms, off_ms) = match &state_inner.pattern {
                            BuzzerPattern::DisconnectedBeep(timing) => (timing.on_ms, timing.off_ms),
                            BuzzerPattern::CriticalBeep(timing) => (timing.on_ms, timing.off_ms),
                            _ => (0, 0),
                        };

                        // Guard against zero duration to avoid division by zero
                        if on_ms + off_ms == 0 {
                            false  // No beep if duration is 0
                        } else {
                            let cycle_duration = on_ms + off_ms;
                            let cycle_pos = elapsed % cycle_duration;
                            cycle_pos < on_ms
                        }
                    }
                };

                // Set buzzer pin state
                if let Some(ref mut bz) = buzzer {
                    bz.set_state(should_beep);

                    // Log state transitions
                    if should_beep && !buzzer_is_on {
                        match &state_inner.pattern {
                            BuzzerPattern::ReconnectionHappy { frequency_hz } => {
                                //eprintln!("[BuzzerThread] >>> Buzzer HAPPY BEEP START (elapsed={}ms, 3-beep pattern with PWM @{}Hz)", elapsed, frequency_hz);
                            }
                            _ => {
                                //eprintln!("[BuzzerThread] >>> Buzzer beep phase START (elapsed={}ms)", elapsed);
                            }
                        }
                        buzzer_is_on = true;
                    } else if !should_beep && buzzer_is_on {
                        //eprintln!("[BuzzerThread] >>> Buzzer silent phase START (elapsed={}ms)", elapsed);
                        buzzer_is_on = false;
                    }
                }
            }
        }
    }

    eprintln!("[BuzzerThread] Buzzer control thread exited cleanly");
}
