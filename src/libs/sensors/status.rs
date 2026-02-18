// Sensor status tracking and LED control logic

use crate::libs::config::SensorAlarmConfig;
use super::reader::SensorStatus;

/// Threshold state for a sensor based on temperature
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorThreshold {
    /// Sensor is disconnected
    Disconnected,
    /// Just reconnected (will blink red 5 times)
    Reconnecting,
    /// Normal operating temperature
    Normal,
    /// Above high alarm threshold (38°C)
    HighAlarm,
    /// Below low alarm threshold (35°C)
    LowAlarm,
    /// Above critical high threshold (40°C) - should blink
    CriticalHigh,
    /// Below critical low threshold (32°C) - should blink
    CriticalLow,
}

/// State of a single sensor line
#[derive(Debug, Clone)]
pub struct SensorLineState {
    /// Current sensor status
    pub status: SensorStatus,
    /// Consecutive read failures
    pub failure_count: u8,
    /// Just reconnected (triggers reconnect animation)
    pub had_reconnect: bool,
    /// Reconnect animation counter (5 blinks)
    pub reconnect_blinks: u8,
    /// Current threshold state
    pub threshold: SensorThreshold,
}

impl SensorLineState {
    /// Create a new sensor line state (starts as disconnected)
    pub fn new() -> Self {
        Self {
            status: SensorStatus::Disconnected,
            failure_count: 0,
            had_reconnect: false,
            reconnect_blinks: 0,
            threshold: SensorThreshold::Disconnected,
        }
    }

    /// Update sensor state with new reading
    pub fn update(&mut self, status: SensorStatus, failure_threshold: u8, thresholds: &SensorAlarmConfig) {
        match status {
            SensorStatus::Connected(temp) => {
                // Successful read - reset failure count
                let was_disconnected = self.failure_count >= failure_threshold;

                self.status = SensorStatus::Connected(temp);
                self.failure_count = 0;

                // If we just recovered from disconnection, trigger reconnect animation
                if was_disconnected {
                    self.had_reconnect = true;
                    self.reconnect_blinks = 0;
                    self.threshold = SensorThreshold::Reconnecting;
                } else {
                    // Normal operation - calculate threshold based on temperature
                    self.threshold = Self::calculate_threshold_from_config(temp, thresholds);
                }
            }
            SensorStatus::Disconnected | SensorStatus::Error => {
                // Read failed - increment failure counter
                self.failure_count += 1;

                // If we've exceeded threshold, mark as disconnected
                if self.failure_count >= failure_threshold {
                    self.status = SensorStatus::Disconnected;
                    self.threshold = SensorThreshold::Disconnected;
                    self.had_reconnect = false;
                    self.reconnect_blinks = 0;
                }
            }
        }
    }

    /// Calculate temperature threshold based on provided config
    fn calculate_threshold_from_config(temp: f32, thresholds: &SensorAlarmConfig) -> SensorThreshold {
        if temp >= thresholds.critical_high_celsius {
            SensorThreshold::CriticalHigh
        } else if temp >= thresholds.high_alarm_celsius {
            SensorThreshold::HighAlarm
        } else if temp <= thresholds.critical_low_celsius {
            SensorThreshold::CriticalLow
        } else if temp <= thresholds.low_alarm_celsius {
            SensorThreshold::LowAlarm
        } else {
            SensorThreshold::Normal
        }
    }

    /// Get LED state (green, red) based on current threshold and blink cycle
    /// blink_cycle: 0-7 counter for blinking effects
    pub fn get_led_state(&mut self, blink_cycle: u8) -> (bool, bool) {
        match self.threshold {
            SensorThreshold::Disconnected => {
                // Disconnected: ALL OFF
                (false, false)
            }
            SensorThreshold::Reconnecting => {
                // Blink red 5 times (10 blink cycles = 5 on, 5 off)
                // Blink pattern: on for 2 cycles, off for 2 cycles, repeat
                if self.reconnect_blinks < 10 {
                    self.reconnect_blinks += 1;
                    // First 5 cycles = blink red
                    let red = self.reconnect_blinks % 4 < 2; // Blink: on 2, off 2
                    (false, red)
                } else {
                    // After 5 blinks, transition to normal/alarm state
                    // For now, show red (will transition to normal on next read)
                    self.threshold = SensorThreshold::Normal;
                    (false, true) // Red ON temporarily
                }
            }
            SensorThreshold::Normal => {
                // Normal: GREEN ON
                (true, false)
            }
            SensorThreshold::HighAlarm | SensorThreshold::LowAlarm => {
                // Alarm: RED ON
                (false, true)
            }
            SensorThreshold::CriticalHigh | SensorThreshold::CriticalLow => {
                // Critical: RED BLINKING (4 on, 4 off)
                let red = blink_cycle < 4;
                (false, red)
            }
        }
    }
}

impl Default for SensorLineState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_thresholds() -> SensorAlarmConfig {
        SensorAlarmConfig {
            critical_low_celsius: 32.0,
            low_alarm_celsius: 0.0,     // disabled - defaults
            warning_low_celsius: 34.0,
            warning_high_celsius: 39.0,
            high_alarm_celsius: 100.0,  // disabled - defaults
            critical_high_celsius: 40.0,
        }
    }

    #[test]
    fn test_sensor_line_starts_disconnected() {
        let state = SensorLineState::new();
        assert_eq!(state.threshold, SensorThreshold::Disconnected);
        assert_eq!(state.failure_count, 0);
    }

    #[test]
    fn test_temperature_threshold_normal() {
        let thresholds = default_thresholds();
        let threshold = SensorLineState::calculate_threshold_from_config(37.0, &thresholds);
        assert_eq!(threshold, SensorThreshold::Normal);
    }

    #[test]
    fn test_temperature_threshold_high_alarm() {
        // Use custom thresholds with alarm zone enabled to test HighAlarm code path
        let thresholds = SensorAlarmConfig {
            critical_low_celsius: 32.0,
            low_alarm_celsius: 35.0,
            warning_low_celsius: 34.0,
            warning_high_celsius: 39.0,
            high_alarm_celsius: 38.0,
            critical_high_celsius: 40.0,
        };
        let threshold = SensorLineState::calculate_threshold_from_config(38.5, &thresholds);
        assert_eq!(threshold, SensorThreshold::HighAlarm);
    }

    #[test]
    fn test_temperature_threshold_normal_with_disabled_alarm() {
        // With disabled alarm thresholds, a value like 38.5 is Normal (not HighAlarm)
        let thresholds = default_thresholds();
        let threshold = SensorLineState::calculate_threshold_from_config(38.5, &thresholds);
        assert_eq!(threshold, SensorThreshold::Normal);
    }

    #[test]
    fn test_temperature_threshold_critical_high() {
        let thresholds = default_thresholds();
        let threshold = SensorLineState::calculate_threshold_from_config(41.0, &thresholds);
        assert_eq!(threshold, SensorThreshold::CriticalHigh);
    }

    #[test]
    fn test_led_state_disconnected() {
        let mut state = SensorLineState::new();
        let (green, red) = state.get_led_state(0);
        assert!(!green && !red, "Disconnected should show all OFF");
    }

    #[test]
    fn test_led_state_normal() {
        let mut state = SensorLineState::new();
        state.threshold = SensorThreshold::Normal;
        let (green, red) = state.get_led_state(0);
        assert!(green && !red, "Normal should show GREEN ON");
    }

    #[test]
    fn test_led_state_high_alarm() {
        let mut state = SensorLineState::new();
        state.threshold = SensorThreshold::HighAlarm;
        let (green, red) = state.get_led_state(0);
        assert!(!green && red, "HighAlarm should show RED ON");
    }

    #[test]
    fn test_led_state_critical_high_blink() {
        let mut state = SensorLineState::new();
        state.threshold = SensorThreshold::CriticalHigh;

        // Test blinking pattern: 4 on, 4 off
        let (_, red1) = state.get_led_state(0); // Blink cycle 0
        let (_, red2) = state.get_led_state(4); // Blink cycle 4
        assert!(red1, "Critical should blink red (cycle 0-3)");
        assert!(!red2, "Critical should blink red (cycle 4-7)");
    }

    #[test]
    fn test_update_successful_read() {
        let mut state = SensorLineState::new();
        state.failure_count = 2; // Simulate some failures (but not enough to disconnect)
        let thresholds = default_thresholds();

        state.update(SensorStatus::Connected(37.0), 3, &thresholds);
        assert_eq!(state.failure_count, 0, "Successful read should reset failure count");
        assert_eq!(state.threshold, SensorThreshold::Normal, "Should be Normal after successful read");
    }

    #[test]
    fn test_update_reconnection_trigger() {
        let mut state = SensorLineState::new();
        // Simulate failed sensor
        state.failure_count = 5;
        state.status = SensorStatus::Disconnected;
        state.threshold = SensorThreshold::Disconnected;
        let thresholds = default_thresholds();

        // Now it reconnects
        state.update(SensorStatus::Connected(37.0), 3, &thresholds);
        assert!(state.had_reconnect, "Should trigger reconnect on recovery");
        assert_eq!(state.threshold, SensorThreshold::Reconnecting);
    }
}
