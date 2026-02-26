//! Alarm state machine implementation

use serde::{Deserialize, Serialize};
use std::fmt;

/// Alarm state - represents the current condition of a monitored value
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlarmState {
    /// Sensor has never been successfully connected (no alarm on startup)
    NeverConnected,
    /// Sensor/device was connected but is now disconnected or not responding
    Disconnected,
    /// Just reconnected; shows reconnection animation (blinking red)
    Reconnecting,
    /// Normal operating condition - all values within safe range
    Normal,
    /// Warning condition - value between normal and critical ranges
    Warning,
    /// Critical condition - value far outside safe range, urgent response needed
    Critical,
}

impl fmt::Display for AlarmState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AlarmState::NeverConnected => write!(f, "NEVER_CONNECTED"),
            AlarmState::Disconnected => write!(f, "DISCONNECTED"),
            AlarmState::Reconnecting => write!(f, "RECONNECTING"),
            AlarmState::Normal => write!(f, "NORMAL"),
            AlarmState::Warning => write!(f, "WARNING"),
            AlarmState::Critical => write!(f, "CRITICAL"),
        }
    }
}

/// State machine for alarm tracking
#[derive(Debug, Clone)]
pub struct AlarmStateMachine {
    /// Current alarm state
    pub current: AlarmState,

    /// Previous alarm state (for detecting transitions)
    pub previous: AlarmState,

    /// Number of consecutive read failures
    pub failure_count: u8,

    /// Reconnect animation counter (0-N, incremented each cycle)
    pub reconnect_cycle: u8,
}

impl AlarmStateMachine {
    /// Create a new state machine (starts NeverConnected)
    pub fn new() -> Self {
        Self {
            current: AlarmState::NeverConnected,
            previous: AlarmState::NeverConnected,
            failure_count: 0,
            reconnect_cycle: 0,
        }
    }

    /// Update state based on read success/failure
    /// Returns true if state changed
    pub fn update_from_read_result(&mut self, success: bool, failure_threshold: u8) -> bool {
        let old_state = self.current;

        if success {
            // Successful read - reset failure count
            self.failure_count = 0;

            // Check if we were previously in a disconnected state
            match self.current {
                AlarmState::Disconnected => {
                    // Was connected before, now reconnecting
                    self.current = AlarmState::Reconnecting;
                    self.reconnect_cycle = 0;
                }
                AlarmState::NeverConnected => {
                    // First successful connection - go directly to reconnecting animation
                    self.current = AlarmState::Reconnecting;
                    self.reconnect_cycle = 0;
                }
                _ => {
                    // Already in a normal state, no change needed
                }
            }
        } else {
            // Failed read - increment failure counter
            self.failure_count += 1;

            // If we've exceeded threshold and we're not already disconnected
            if self.failure_count >= failure_threshold {
                // Only transition to Disconnected if we were previously connected
                // NeverConnected stays NeverConnected (no alarm)
                match self.current {
                    AlarmState::NeverConnected => {
                        // Stay in NeverConnected - don't alarm on startup
                        self.reconnect_cycle = 0;
                    }
                    AlarmState::Disconnected => {
                        // Already disconnected
                        self.reconnect_cycle = 0;
                    }
                    _ => {
                        // Was in a normal/warning/alarm/critical state - now disconnected
                        self.current = AlarmState::Disconnected;
                        self.reconnect_cycle = 0;
                    }
                }
            }
        }

        old_state != self.current
    }

    /// Update state based on threshold evaluation
    /// Should be called after a successful read to classify the temperature
    /// Note: is_alarm range is absorbed into Warning (Alarm state was removed)
    pub fn update_from_threshold(&mut self, is_critical: bool, is_alarm: bool, is_warning: bool) {
        // Priority: critical > alarm/warning > normal
        // The old "Alarm" state was removed; alarm range now maps to Warning
        let new_state = if is_critical {
            AlarmState::Critical
        } else if is_alarm || is_warning {
            AlarmState::Warning
        } else {
            AlarmState::Normal
        };

        self.previous = self.current;
        self.current = new_state;
    }

    /// Progress reconnection animation
    pub fn advance_reconnect_cycle(&mut self) {
        self.reconnect_cycle = self.reconnect_cycle.wrapping_add(1);

        // After 10 cycles (5 on, 5 off with 2-cycle pattern), exit reconnecting
        if self.reconnect_cycle >= 10 {
            // Don't change state yet - let next temperature reading set the actual state
            self.reconnect_cycle = 10; // Cap at 10
        }
    }

    /// Check if reconnect animation is still active
    pub fn is_reconnecting(&self) -> bool {
        self.current == AlarmState::Reconnecting && self.reconnect_cycle < 10
    }

    /// Check if state just changed
    pub fn state_changed(&self) -> bool {
        self.current != self.previous
    }

    /// Check if we just entered an alarm/critical state
    pub fn just_alarmed(&self) -> bool {
        self.state_changed()
            && self.current == AlarmState::Critical
    }

    /// Check if we just entered a warning state
    pub fn just_warned(&self) -> bool {
        self.state_changed() && self.current == AlarmState::Warning
    }

    /// Check if we just recovered to normal
    pub fn just_recovered(&self) -> bool {
        self.state_changed() && self.current == AlarmState::Normal
    }

    /// Check if we just entered reconnecting state
    pub fn just_reconnecting(&self) -> bool {
        self.state_changed() && self.current == AlarmState::Reconnecting
    }
}

impl Default for AlarmStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_starts_never_connected() {
        let sm = AlarmStateMachine::new();
        assert_eq!(sm.current, AlarmState::NeverConnected);
        assert_eq!(sm.failure_count, 0);
    }

    #[test]
    fn test_never_connected_stays_never_connected_on_failures() {
        let mut sm = AlarmStateMachine::new();
        assert_eq!(sm.current, AlarmState::NeverConnected);

        // Multiple failures in NeverConnected state
        sm.update_from_read_result(false, 3);
        assert_eq!(sm.failure_count, 1);
        sm.update_from_read_result(false, 3);
        assert_eq!(sm.failure_count, 2);
        sm.update_from_read_result(false, 3);
        assert_eq!(sm.failure_count, 3);

        // Should still be NeverConnected, NOT Disconnected (no alarm on startup)
        assert_eq!(sm.current, AlarmState::NeverConnected);
    }

    #[test]
    fn test_failure_count_accumulates() {
        let mut sm = AlarmStateMachine::new();
        // Transition out of NeverConnected first
        sm.update_from_read_result(true, 3);
        assert_eq!(sm.current, AlarmState::Reconnecting);

        sm.previous = sm.current;
        sm.update_from_read_result(false, 3);
        assert_eq!(sm.failure_count, 1);
        sm.update_from_read_result(false, 3);
        assert_eq!(sm.failure_count, 2);
        sm.update_from_read_result(false, 3);
        assert_eq!(sm.failure_count, 3);
        assert_eq!(sm.current, AlarmState::Disconnected);
    }

    #[test]
    fn test_success_resets_failures() {
        let mut sm = AlarmStateMachine::new();
        sm.update_from_read_result(false, 3);
        sm.update_from_read_result(false, 3);
        assert_eq!(sm.failure_count, 2);

        sm.update_from_read_result(true, 3);
        assert_eq!(sm.failure_count, 0);
    }

    #[test]
    fn test_reconnection_trigger() {
        let mut sm = AlarmStateMachine::new();
        // Start NeverConnected
        assert_eq!(sm.current, AlarmState::NeverConnected);

        // Successful read triggers reconnecting animation
        sm.update_from_read_result(true, 3);
        assert_eq!(sm.current, AlarmState::Reconnecting);
    }

    #[test]
    fn test_threshold_updates() {
        let mut sm = AlarmStateMachine::new();
        sm.failure_count = 0; // Force out of disconnected state

        sm.update_from_threshold(false, false, false);
        assert_eq!(sm.current, AlarmState::Normal);

        sm.update_from_threshold(false, false, true);
        assert_eq!(sm.current, AlarmState::Warning);

        sm.update_from_threshold(true, false, false);
        assert_eq!(sm.current, AlarmState::Critical);
    }

    #[test]
    fn test_state_changed_detection() {
        let mut sm = AlarmStateMachine::new();
        assert!(!sm.state_changed()); // No change yet

        sm.current = AlarmState::Normal;
        assert!(sm.state_changed());
    }

    #[test]
    fn test_just_alarmed() {
        let mut sm = AlarmStateMachine::new();
        sm.current = AlarmState::Normal;
        sm.previous = AlarmState::Normal;

        // just_alarmed() checks for Critical state transition
        sm.update_from_threshold(true, false, false);
        assert!(sm.just_alarmed());

        sm.previous = sm.current; // Clear the transition
        sm.update_from_threshold(true, false, false);
        assert!(!sm.just_alarmed());
    }

    #[test]
    fn test_reconnect_cycle_advancement() {
        let mut sm = AlarmStateMachine::new();
        sm.current = AlarmState::Reconnecting;
        sm.reconnect_cycle = 0;

        assert!(sm.is_reconnecting());
        for _ in 0..10 {
            sm.advance_reconnect_cycle();
        }
        assert_eq!(sm.reconnect_cycle, 10);
        assert!(!sm.is_reconnecting());
    }
}
