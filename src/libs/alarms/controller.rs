//! Main alarm controller - coordinates state machine, thresholds, LED control, and callbacks

use crate::libs::alarms::callbacks::{AlarmCallback, AlarmEvent};
use crate::libs::alarms::color::{BlinkPattern, LedColor, LedState};
use crate::libs::alarms::state::AlarmStateMachine;
use crate::libs::alarms::threshold::AlarmThreshold;
use std::fmt;
use std::sync::Arc;

/// Main alarm controller
/// Manages all aspects of alarm handling for a single monitored value
pub struct AlarmController {
    /// Threshold configuration
    thresholds: AlarmThreshold,

    /// State machine
    state_machine: AlarmStateMachine,

    /// Registered callbacks for events
    callbacks: Vec<Arc<dyn AlarmCallback>>,

    /// Number of consecutive failures before marking disconnected
    failure_threshold: u8,

    /// Configurable reconnect animation cycles
    reconnect_blinks: u8,
}

impl fmt::Debug for AlarmController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlarmController")
            .field("thresholds", &self.thresholds)
            .field("state_machine", &self.state_machine)
            .field("failure_threshold", &self.failure_threshold)
            .field("reconnect_blinks", &self.reconnect_blinks)
            .field("callbacks_count", &self.callbacks.len())
            .finish()
    }
}

impl AlarmController {
    /// Create a new alarm controller
    pub fn new(thresholds: AlarmThreshold, failure_threshold: u8, reconnect_blinks: u8) -> Self {
        Self {
            thresholds,
            state_machine: AlarmStateMachine::new(),
            callbacks: Vec::new(),
            failure_threshold,
            reconnect_blinks,
        }
    }

    /// Update with a new reading value
    /// Returns the LED state (color and blink pattern) for this cycle
    pub fn update(&mut self, value: f32) -> LedState {
        // Mark read as successful
        self.state_machine
            .update_from_read_result(true, self.failure_threshold);

        // Evaluate against thresholds
        let is_critical = self.thresholds.is_critical(value);
        let is_alarm = self.thresholds.is_alarm(value);
        let is_warning = self.thresholds.is_warning(value);

        self.state_machine
            .update_from_threshold(is_critical, is_alarm, is_warning);

        // Fire callbacks
        self.fire_callbacks();

        // Return LED state for this reading
        self.get_led_state()
    }

    /// Mark a read failure
    /// Returns the LED state (for disconnection state)
    pub fn mark_read_failure(&mut self) -> LedState {
        self.state_machine
            .update_from_read_result(false, self.failure_threshold);

        // Fire callbacks if state changed
        self.fire_callbacks();

        self.get_led_state()
    }

    /// Get current state
    pub fn state(&self) -> crate::libs::alarms::state::AlarmState {
        self.state_machine.current
    }

    /// Check if we just entered the Reconnecting state
    pub fn just_reconnecting(&self) -> bool {
        self.state_machine.just_reconnecting()
    }

    /// Get current LED state (color and blink pattern)
    pub fn get_led_state(&self) -> LedState {
        match self.state_machine.current {
            crate::libs::alarms::state::AlarmState::NeverConnected => {
                // Never connected: LED off (no alarm on startup)
                LedState::new(LedColor::Off, BlinkPattern::Steady)
            }
            crate::libs::alarms::state::AlarmState::Disconnected => {
                // Disconnected: blinking red (slow) with buzzer beeping
                LedState::new(LedColor::Red, BlinkPattern::BlinkSlow)
            }
            crate::libs::alarms::state::AlarmState::Reconnecting => {
                // Reconnecting: blinking red with configurable cycles
                // Blink pattern: 2 on, 2 off for first N*2 cycles
                let should_blink = self.state_machine.reconnect_cycle % 4 < 2;
                if should_blink {
                    LedState::new(LedColor::Red, BlinkPattern::Steady)
                } else {
                    LedState::new(LedColor::Off, BlinkPattern::Steady)
                }
            }
            crate::libs::alarms::state::AlarmState::Normal => {
                // Normal: steady green
                LedState::new(LedColor::Green, BlinkPattern::Steady)
            }
            crate::libs::alarms::state::AlarmState::Warning => {
                // Warning: blinking yellow (slow blink) - LEDG + LEDR combined
                LedState::new(LedColor::Yellow, BlinkPattern::BlinkSlow)
            }
            crate::libs::alarms::state::AlarmState::Alarm => {
                // Alarm: steady red
                LedState::new(LedColor::Red, BlinkPattern::Steady)
            }
            crate::libs::alarms::state::AlarmState::Critical => {
                // Critical: blinking red (fast blink)
                LedState::new(LedColor::Red, BlinkPattern::BlinkFast)
            }
        }
    }

    /// Register a callback for events
    pub fn register_callback(&mut self, callback: Arc<dyn AlarmCallback>) {
        self.callbacks.push(callback);
    }

    /// Progress reconnection animation (call every cycle)
    pub fn advance_reconnect_animation(&mut self) {
        self.state_machine.advance_reconnect_cycle();

        // After animation completes, transition out of reconnecting state
        if self.state_machine.reconnect_cycle >= (self.reconnect_blinks * 2) {
            // Next temperature reading will set actual state
        }
    }

    /// Fire callbacks for state changes
    fn fire_callbacks(&self) {
        if self.state_machine.state_changed() {
            // State changed event
            let event = AlarmEvent::StateChanged {
                from: self.state_machine.previous,
                to: self.state_machine.current,
            };

            for callback in &self.callbacks {
                callback.on_event(event.clone());
            }

            // Also fire specific events for easier filtering
            match self.state_machine.current {
                crate::libs::alarms::state::AlarmState::Warning => {
                    let event = AlarmEvent::Warning { value: 0.0 };
                    for callback in &self.callbacks {
                        callback.on_event(event.clone());
                    }
                }
                crate::libs::alarms::state::AlarmState::Alarm => {
                    let event = AlarmEvent::Alarm { value: 0.0 };
                    for callback in &self.callbacks {
                        callback.on_event(event.clone());
                    }
                }
                crate::libs::alarms::state::AlarmState::Critical => {
                    let event = AlarmEvent::Critical { value: 0.0 };
                    for callback in &self.callbacks {
                        callback.on_event(event.clone());
                    }
                }
                crate::libs::alarms::state::AlarmState::Reconnecting => {
                    let event = AlarmEvent::Reconnected;
                    for callback in &self.callbacks {
                        callback.on_event(event.clone());
                    }
                }
                crate::libs::alarms::state::AlarmState::Disconnected => {
                    let event = AlarmEvent::Disconnected;
                    for callback in &self.callbacks {
                        callback.on_event(event.clone());
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::alarms::callbacks::LoggingCallback;

    #[test]
    fn test_controller_creation() {
        let thresholds = AlarmThreshold::default_medical();
        let controller = AlarmController::new(thresholds, 3, 5);
        assert_eq!(
            controller.state(),
            crate::libs::alarms::state::AlarmState::NeverConnected
        );
    }

    #[test]
    fn test_normal_temperature_returns_green() {
        let mut controller =
            AlarmController::new(AlarmThreshold::default_medical(), 3, 5);
        let led_state = controller.update(37.0);
        assert_eq!(led_state.color, LedColor::Green);
        assert_eq!(led_state.pattern, BlinkPattern::Steady);
    }

    #[test]
    fn test_high_temperature_alarm() {
        let mut controller =
            AlarmController::new(AlarmThreshold::default_medical(), 3, 5);
        let led_state = controller.update(38.5); // Above high_alarm (38.0)
        assert_eq!(led_state.color, LedColor::Red);
        assert_eq!(led_state.pattern, BlinkPattern::Steady);
    }

    #[test]
    fn test_alarm_temperature_returns_red() {
        let mut controller =
            AlarmController::new(AlarmThreshold::default_medical(), 3, 5);
        let led_state = controller.update(34.0); // Below low_alarm (35.0), above critical_low (32.0)
        assert_eq!(led_state.color, LedColor::Red);
        assert_eq!(led_state.pattern, BlinkPattern::Steady);
    }

    #[test]
    fn test_critical_temperature_returns_blinking_red() {
        let mut controller =
            AlarmController::new(AlarmThreshold::default_medical(), 3, 5);
        let led_state = controller.update(31.0); // Below critical_low (32.0)
        assert_eq!(led_state.color, LedColor::Red);
        assert_eq!(led_state.pattern, BlinkPattern::BlinkFast);
    }

    #[test]
    fn test_read_failures_trigger_disconnection() {
        let mut controller =
            AlarmController::new(AlarmThreshold::default_medical(), 3, 5);
        // First, get out of NeverConnected state
        let _ = controller.update(37.0);  // Successful read moves to Reconnecting
        // Now simulate disconnection
        controller.mark_read_failure();
        controller.mark_read_failure();
        controller.mark_read_failure();
        assert_eq!(
            controller.state(),
            crate::libs::alarms::state::AlarmState::Disconnected
        );
    }

    #[test]
    fn test_reconnection_after_failures() {
        let mut controller =
            AlarmController::new(AlarmThreshold::default_medical(), 3, 5);
        // First successful read to get out of NeverConnected
        let _ = controller.update(37.0);
        // Now simulate disconnection
        controller.mark_read_failure();
        controller.mark_read_failure();
        controller.mark_read_failure();

        // Verify we're disconnected
        assert_eq!(
            controller.state(),
            crate::libs::alarms::state::AlarmState::Disconnected
        );

        // First successful read triggers reconnecting, but update_from_threshold
        // immediately transitions to Normal (since 37.0°C is normal)
        let led_state = controller.update(37.0);
        assert_eq!(
            controller.state(),
            crate::libs::alarms::state::AlarmState::Normal
        );
        assert_eq!(led_state.color, LedColor::Green);
    }

    #[test]
    fn test_callback_registration() {
        let mut controller =
            AlarmController::new(AlarmThreshold::default_medical(), 3, 5);
        let callback = Arc::new(LoggingCallback::default());
        controller.register_callback(callback);
        assert_eq!(controller.callbacks.len(), 1);
    }

    #[test]
    fn test_25_celsius_triggers_critical_blinking_red() {
        // 25°C is CRITICAL (below critical_low 32°C)
        // So it should show blinking red, not warning
        let mut controller =
            AlarmController::new(AlarmThreshold::default_medical(), 3, 5);
        let led_state = controller.update(25.0);

        // 25°C < critical_low (32°C), so it's critical
        assert_eq!(
            controller.state(),
            crate::libs::alarms::state::AlarmState::Critical
        );
        assert_eq!(led_state.color, LedColor::Red);
        assert_eq!(led_state.pattern, BlinkPattern::BlinkFast);
    }

    #[test]
    fn test_33_5_celsius_triggers_alarm_red_led() {
        // 33.5°C is ALARM (below low_alarm 35°C)
        // Even though it's also below warning_low (34°C), alarm takes priority
        // This should show red steady
        let mut controller =
            AlarmController::new(AlarmThreshold::default_medical(), 3, 5);
        let led_state = controller.update(33.5);

        // 33.5°C < low_alarm (35°C), so it's alarm (priority over warning)
        assert_eq!(
            controller.state(),
            crate::libs::alarms::state::AlarmState::Alarm
        );
        assert_eq!(led_state.color, LedColor::Red);
        assert_eq!(led_state.pattern, BlinkPattern::Steady);
    }
}
