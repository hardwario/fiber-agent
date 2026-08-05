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

    /// Number of consecutive successful reads before exiting NeverConnected
    warmup_threshold: u8,

    /// Configurable reconnect animation cycles
    reconnect_blinks: u8,
}

impl fmt::Debug for AlarmController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlarmController")
            .field("thresholds", &self.thresholds)
            .field("state_machine", &self.state_machine)
            .field("failure_threshold", &self.failure_threshold)
            .field("warmup_threshold", &self.warmup_threshold)
            .field("reconnect_blinks", &self.reconnect_blinks)
            .field("callbacks_count", &self.callbacks.len())
            .finish()
    }
}

impl AlarmController {
    /// Create a new alarm controller
    pub fn new(
        thresholds: AlarmThreshold,
        failure_threshold: u8,
        reconnect_blinks: u8,
        warmup_threshold: u8,
    ) -> Self {
        Self {
            thresholds,
            state_machine: AlarmStateMachine::new(),
            callbacks: Vec::new(),
            failure_threshold,
            warmup_threshold,
            reconnect_blinks,
        }
    }

    /// Update with a new reading value
    /// Returns the LED state (color and blink pattern) for this cycle
    pub fn update(&mut self, value: f32) -> LedState {
        // Mark read as successful
        self.state_machine.update_from_read_result(
            true,
            self.failure_threshold,
            self.warmup_threshold,
        );

        // Evaluate against thresholds
        let is_critical = self.thresholds.is_critical(value);
        let is_alarm = self.thresholds.is_alarm(value);
        let is_warning = self.thresholds.is_warning(value);

        self.state_machine
            .update_from_threshold(is_critical, is_alarm, is_warning);

        // Fire callbacks with actual temperature value
        self.fire_callbacks(Some(value));

        // Return LED state for this reading
        self.get_led_state()
    }

    /// Mark a read failure
    /// Returns the LED state (for disconnection state)
    pub fn mark_read_failure(&mut self) -> LedState {
        self.state_machine.update_from_read_result(
            false,
            self.failure_threshold,
            self.warmup_threshold,
        );

        // Fire callbacks if state changed (no temperature value on failure)
        self.fire_callbacks(None);

        self.get_led_state()
    }

    /// Get current state
    pub fn state(&self) -> crate::libs::alarms::state::AlarmState {
        self.state_machine.current
    }

    /// Update alarm thresholds (for hot reload)
    pub fn update_thresholds(&mut self, new_thresholds: AlarmThreshold) {
        self.thresholds = new_thresholds;
    }

    /// Forget everything observed so far, back to `NeverConnected`.
    ///
    /// Thresholds, callbacks and the failure/warmup/reconnect configuration are
    /// kept — only what the controller has *seen* is dropped.
    ///
    /// Used when the device enters standby. The sensor rails are switched off
    /// there and no readings are taken, so a latched `Critical` or `Disconnected`
    /// is an assertion the device can no longer justify: it would keep the buzzer
    /// going and, on resume, either report a transition out of a state nothing
    /// observed or stay silently latched. `NeverConnected` is the state the
    /// machine boots in precisely because it raises no alarm, so a resume warms up
    /// from live readings exactly like a fresh start.
    pub fn reset(&mut self) {
        self.state_machine = AlarmStateMachine::new();
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
    /// `value` is the actual temperature reading (None for read failures)
    fn fire_callbacks(&self, value: Option<f32>) {
        if self.state_machine.state_changed() {
            // State changed event
            let event = AlarmEvent::StateChanged {
                from: self.state_machine.previous,
                to: self.state_machine.current,
            };

            for callback in &self.callbacks {
                callback.on_event(event.clone());
            }

            let temp = value.unwrap_or(0.0);

            // Also fire specific events for easier filtering
            match self.state_machine.current {
                crate::libs::alarms::state::AlarmState::Warning => {
                    let event = AlarmEvent::Warning { value: temp };
                    for callback in &self.callbacks {
                        callback.on_event(event.clone());
                    }
                }
                crate::libs::alarms::state::AlarmState::Critical => {
                    let event = AlarmEvent::Critical { value: temp };
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
    use crate::libs::alarms::state::AlarmState;

    #[test]
    fn test_controller_creation() {
        let thresholds = AlarmThreshold::default_medical();
        let controller = AlarmController::new(thresholds, 3, 5, 1);
        assert_eq!(
            controller.state(),
            crate::libs::alarms::state::AlarmState::NeverConnected
        );
    }

    #[test]
    fn reset_returns_a_critical_controller_to_never_connected() {
        // What entering standby does. A latched Critical would keep the buzzer
        // going on a device the operator has switched off, and the rails are down
        // so nothing can justify it any more.
        let mut controller = AlarmController::new(AlarmThreshold::default_medical(), 3, 5, 1);
        controller.update(45.0);
        assert_eq!(
            controller.state(),
            crate::libs::alarms::state::AlarmState::Critical
        );

        controller.reset();

        assert_eq!(
            controller.state(),
            crate::libs::alarms::state::AlarmState::NeverConnected
        );
        let led = controller.get_led_state();
        assert_eq!(led.color, LedColor::Off, "a reset line raises no alarm");
        assert_eq!(led.pattern, BlinkPattern::Steady);
    }

    #[test]
    fn reset_keeps_thresholds_so_a_resume_still_alarms() {
        // Only what the controller has *seen* is dropped. If the temperature is
        // still critical when the device wakes, it must say so again.
        let mut controller = AlarmController::new(AlarmThreshold::default_medical(), 3, 5, 1);
        controller.update(45.0);
        controller.reset();

        let led = controller.update(45.0);
        assert_eq!(
            controller.state(),
            crate::libs::alarms::state::AlarmState::Critical,
            "the thresholds survived the reset"
        );
        assert_eq!(led.color, LedColor::Red);
    }

    #[test]
    fn reset_clears_a_disconnected_line_too() {
        // The rails are switched off in standby, so every line would otherwise be
        // latched Disconnected — eight red LEDs and a beeping device.
        let mut controller = AlarmController::new(AlarmThreshold::default_medical(), 1, 1, 1);
        controller.update(37.0);
        controller.mark_read_failure();
        assert_eq!(
            controller.state(),
            crate::libs::alarms::state::AlarmState::Disconnected
        );

        controller.reset();
        assert_eq!(
            controller.state(),
            crate::libs::alarms::state::AlarmState::NeverConnected
        );
    }

    #[test]
    fn test_normal_temperature_returns_green() {
        let mut controller = AlarmController::new(AlarmThreshold::default_medical(), 3, 5, 1);
        let led_state = controller.update(37.0);
        assert_eq!(led_state.color, LedColor::Green);
        assert_eq!(led_state.pattern, BlinkPattern::Steady);
    }

    #[test]
    fn test_high_temperature_warning() {
        // 39.5°C is above warning_high (39.0) but below critical_high (40.0)
        // In the 4-level system, this is Warning (yellow, slow blink)
        let mut controller = AlarmController::new(AlarmThreshold::default_medical(), 3, 5, 1);
        let led_state = controller.update(39.5);
        assert_eq!(led_state.color, LedColor::Yellow);
        assert_eq!(led_state.pattern, BlinkPattern::BlinkSlow);
    }

    #[test]
    fn test_low_temperature_warning() {
        // 33.0°C is below warning_low (34.0) but above critical_low (32.0)
        // In the 4-level system, this is Warning (yellow, slow blink)
        let mut controller = AlarmController::new(AlarmThreshold::default_medical(), 3, 5, 1);
        let led_state = controller.update(33.0);
        assert_eq!(led_state.color, LedColor::Yellow);
        assert_eq!(led_state.pattern, BlinkPattern::BlinkSlow);
    }

    #[test]
    fn test_former_alarm_zone_now_normal() {
        // With alarm thresholds disabled (0.0/100.0), temperatures like 38.5
        // that were previously in the alarm zone are now Normal
        let mut controller = AlarmController::new(AlarmThreshold::default_medical(), 3, 5, 1);
        let led_state = controller.update(38.5);
        assert_eq!(led_state.color, LedColor::Green);
        assert_eq!(led_state.pattern, BlinkPattern::Steady);
    }

    #[test]
    fn test_critical_temperature_returns_blinking_red() {
        let mut controller = AlarmController::new(AlarmThreshold::default_medical(), 3, 5, 1);
        let led_state = controller.update(31.0); // Below critical_low (32.0)
        assert_eq!(led_state.color, LedColor::Red);
        assert_eq!(led_state.pattern, BlinkPattern::BlinkFast);
    }

    #[test]
    fn test_read_failures_trigger_disconnection() {
        let mut controller = AlarmController::new(AlarmThreshold::default_medical(), 3, 5, 1);
        // First, get out of NeverConnected state
        let _ = controller.update(37.0); // Successful read moves to Reconnecting
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
        let mut controller = AlarmController::new(AlarmThreshold::default_medical(), 3, 5, 1);
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
        let mut controller = AlarmController::new(AlarmThreshold::default_medical(), 3, 5, 1);
        let callback = Arc::new(LoggingCallback::default());
        controller.register_callback(callback);
        assert_eq!(controller.callbacks.len(), 1);
    }

    #[test]
    fn test_25_celsius_triggers_critical_blinking_red() {
        // 25°C is CRITICAL (below critical_low 32°C)
        // So it should show blinking red, not warning
        let mut controller = AlarmController::new(AlarmThreshold::default_medical(), 3, 5, 1);
        let led_state = controller.update(25.0);

        // 25°C < critical_low (32°C), so it's critical
        assert_eq!(
            controller.state(),
            crate::libs::alarms::state::AlarmState::Critical
        );
        assert_eq!(led_state.color, LedColor::Red);
        assert_eq!(led_state.pattern, BlinkPattern::BlinkFast);
    }

    /// Records every event a controller fires, so a test can assert on what
    /// reached the LED/buzzer/MQTT callbacks rather than only on final state.
    #[derive(Default)]
    struct RecordingCallback {
        events: std::sync::Mutex<Vec<AlarmEvent>>,
    }

    impl RecordingCallback {
        fn events(&self) -> Vec<AlarmEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl AlarmCallback for RecordingCallback {
        fn on_event(&self, event: AlarmEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    /// Build a controller with the real deployed warm-up of 3 consecutive reads.
    fn warming_up_controller() -> (AlarmController, Arc<RecordingCallback>) {
        let mut controller = AlarmController::new(AlarmThreshold::default_medical(), 3, 5, 3);
        let recorder = Arc::new(RecordingCallback::default());
        controller.register_callback(recorder.clone());
        (controller, recorder)
    }

    #[test]
    fn a_warming_up_line_does_not_alarm_on_a_bus_artefact() {
        // The bug this guards: 127.9375°C (raw 0x07FF) read from a line whose
        // rails have only just come up used to be classified CRITICAL on sample
        // one, sounding the buzzer, because update_from_threshold ignored the
        // NeverConnected warm-up gate.
        let (mut controller, recorder) = warming_up_controller();

        for _ in 0..2 {
            let led = controller.update(127.9375);
            assert_eq!(controller.state(), AlarmState::NeverConnected);
            assert_eq!(led.color, LedColor::Off);
        }

        assert!(
            recorder.events().is_empty(),
            "a line still warming up must fire no events, got {:?}",
            recorder.events()
        );
    }

    #[test]
    fn warmup_defers_classification_until_enough_consecutive_reads() {
        let (mut controller, _recorder) = warming_up_controller();

        controller.update(37.0);
        assert_eq!(controller.state(), AlarmState::NeverConnected);
        controller.update(37.0);
        assert_eq!(controller.state(), AlarmState::NeverConnected);

        // Third consecutive good read completes warm-up and classifies
        controller.update(37.0);
        assert_eq!(controller.state(), AlarmState::Normal);
    }

    #[test]
    fn a_failure_restarts_the_warmup_run() {
        let (mut controller, _recorder) = warming_up_controller();

        controller.update(37.0);
        controller.update(37.0);
        controller.mark_read_failure();
        controller.update(37.0);
        controller.update(37.0);

        // Two good reads since the failure is not yet three
        assert_eq!(controller.state(), AlarmState::NeverConnected);
        controller.update(37.0);
        assert_eq!(controller.state(), AlarmState::Normal);
    }

    #[test]
    fn a_genuine_alarm_after_warmup_still_fires() {
        // Guard against over-suppression: the gate must delay alarms, not mute them.
        let (mut controller, recorder) = warming_up_controller();

        for _ in 0..3 {
            controller.update(37.0);
        }
        assert_eq!(controller.state(), AlarmState::Normal);

        let led = controller.update(45.0);
        assert_eq!(controller.state(), AlarmState::Critical);
        assert_eq!(led.color, LedColor::Red);
        assert_eq!(led.pattern, BlinkPattern::BlinkFast);
        assert!(
            recorder
                .events()
                .iter()
                .any(|e| matches!(e, AlarmEvent::Critical { .. })),
            "a real over-temperature must still raise Critical"
        );
    }

    #[test]
    fn completing_warmup_reports_a_first_connection_edge_not_a_reconnect() {
        // MQTT suppresses transitions touching NeverConnected. If warm-up
        // completion published Reconnecting -> Normal instead, every boot and
        // every standby resume would emit a spurious alarm event.
        let (mut controller, recorder) = warming_up_controller();

        for _ in 0..3 {
            controller.update(37.0);
        }

        let transitions: Vec<_> = recorder
            .events()
            .into_iter()
            .filter_map(|e| match e {
                AlarmEvent::StateChanged { from, to } => Some((from, to)),
                _ => None,
            })
            .collect();

        assert_eq!(
            transitions,
            vec![(AlarmState::NeverConnected, AlarmState::Normal)],
            "warm-up completion must look like a first connection"
        );
    }

    #[test]
    fn a_line_that_only_ever_reads_garbage_stays_dark_and_silent() {
        // The reader rejects 127.9375, so the monitor debounces it as a read
        // failure. A line that never produced a good reading must not alarm —
        // that is what keeps the unpopulated lines quiet on an 8-line device.
        let (mut controller, recorder) = warming_up_controller();

        for _ in 0..10 {
            let led = controller.mark_read_failure();
            assert_eq!(controller.state(), AlarmState::NeverConnected);
            assert_eq!(led.color, LedColor::Off);
        }
        assert!(recorder.events().is_empty());
    }

    #[test]
    fn a_working_line_that_starts_reading_garbage_becomes_disconnected() {
        // Once a probe has proven itself, rejected readings must escalate — a
        // failing sensor is a fault the operator has to see.
        let (mut controller, _recorder) = warming_up_controller();

        for _ in 0..3 {
            controller.update(37.0);
        }
        assert_eq!(controller.state(), AlarmState::Normal);

        for _ in 0..3 {
            controller.mark_read_failure();
        }
        assert_eq!(controller.state(), AlarmState::Disconnected);
    }

    #[test]
    fn test_33_5_celsius_triggers_warning() {
        // 33.5°C is below warning_low (34°C), so it's Warning
        let mut controller = AlarmController::new(AlarmThreshold::default_medical(), 3, 5, 1);
        let led_state = controller.update(33.5);

        assert_eq!(
            controller.state(),
            crate::libs::alarms::state::AlarmState::Warning
        );
        assert_eq!(led_state.color, LedColor::Yellow);
        assert_eq!(led_state.pattern, BlinkPattern::BlinkSlow);
    }
}
