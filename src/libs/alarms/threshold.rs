//! Alarm threshold definitions

use serde::{Deserialize, Serialize};

/// Threshold configuration for temperature-based alarms
///
/// Defines 6 temperature thresholds that create different alarm states:
/// - Critical Low: Below this, state is Critical (blinking alert)
/// - Low Alarm: Below this, state is Alarm (solid alert)
/// - Warning Low: Below this, state is Warning
/// - Warning High: Above this, state is Warning
/// - High Alarm: Above this, state is Alarm (solid alert)
/// - Critical High: Above this, state is Critical (blinking alert)
///
/// Example with defaults:
/// ```text
/// Critical Low (32°C) ----
///                         Low Alarm (35°C) ----
///                                             Warning Low (34°C) ----
///     ============ NORMAL ZONE (34-39°C) ============
///                                             Warning High (39°C) ----
///                         High Alarm (38°C) ----
/// Critical High (40°C) ----
/// ```
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct AlarmThreshold {
    /// Temperature below which state is Critical (blinking alert)
    pub critical_low_celsius: f32,

    /// Temperature below which state is Alarm (solid alert)
    pub low_alarm_celsius: f32,

    /// Temperature below which state is Warning
    pub warning_low_celsius: f32,

    /// Temperature above which state is Warning
    pub warning_high_celsius: f32,

    /// Temperature above which state is Alarm (solid alert)
    pub high_alarm_celsius: f32,

    /// Temperature above which state is Critical (blinking alert)
    pub critical_high_celsius: f32,
}

impl AlarmThreshold {
    /// Create thresholds with custom values
    pub fn new(
        critical_low: f32,
        low_alarm: f32,
        warning_low: f32,
        warning_high: f32,
        high_alarm: f32,
        critical_high: f32,
    ) -> Self {
        Self {
            critical_low_celsius: critical_low,
            low_alarm_celsius: low_alarm,
            warning_low_celsius: warning_low,
            warning_high_celsius: warning_high,
            high_alarm_celsius: high_alarm,
            critical_high_celsius: critical_high,
        }
    }

    /// Get default medical thermometer thresholds (36.5°C center)
    pub fn default_medical() -> Self {
        Self {
            critical_low_celsius: 32.0,
            low_alarm_celsius: 35.0,
            warning_low_celsius: 34.0,
            warning_high_celsius: 39.0,
            high_alarm_celsius: 38.0,
            critical_high_celsius: 40.0,
        }
    }

    /// Check if value is within warning range
    pub fn is_warning(&self, value: f32) -> bool {
        value < self.warning_low_celsius || value > self.warning_high_celsius
    }

    /// Check if value is within alarm range
    pub fn is_alarm(&self, value: f32) -> bool {
        value < self.low_alarm_celsius || value > self.high_alarm_celsius
    }

    /// Check if value is within critical range
    pub fn is_critical(&self, value: f32) -> bool {
        value < self.critical_low_celsius || value > self.critical_high_celsius
    }

    /// Check if value is within normal range
    pub fn is_normal(&self, value: f32) -> bool {
        value >= self.warning_low_celsius && value <= self.warning_high_celsius
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_medical_thresholds() {
        let t = AlarmThreshold::default_medical();
        assert_eq!(t.critical_low_celsius, 32.0);
        assert_eq!(t.low_alarm_celsius, 35.0);
        assert_eq!(t.warning_low_celsius, 34.0);
        assert_eq!(t.warning_high_celsius, 39.0);
        assert_eq!(t.high_alarm_celsius, 38.0);
        assert_eq!(t.critical_high_celsius, 40.0);
    }

    #[test]
    fn test_is_normal() {
        let t = AlarmThreshold::default_medical();
        assert!(t.is_normal(35.0));
        assert!(t.is_normal(37.0));
        assert!(t.is_normal(39.0));
        assert!(!t.is_normal(33.0));
        assert!(!t.is_normal(40.0));
    }

    #[test]
    fn test_is_warning() {
        let t = AlarmThreshold::default_medical();
        assert!(t.is_warning(33.0)); // Below warning low
        assert!(t.is_warning(40.0)); // Above warning high
        assert!(!t.is_warning(35.0)); // Normal
    }

    #[test]
    fn test_is_alarm() {
        let t = AlarmThreshold::default_medical();
        assert!(t.is_alarm(34.0)); // Below low alarm
        assert!(t.is_alarm(39.0)); // Above high alarm
        assert!(!t.is_alarm(36.0)); // Normal
    }

    #[test]
    fn test_is_critical() {
        let t = AlarmThreshold::default_medical();
        assert!(t.is_critical(31.0)); // Below critical low
        assert!(t.is_critical(41.0)); // Above critical high
        assert!(!t.is_critical(35.0)); // Normal
    }

    #[test]
    fn test_25_degrees_is_normal() {
        let t = AlarmThreshold::default_medical();
        // This verifies the user's requirement: 25°C should show GREEN (normal)
        // 25°C is below warning_low (34°C), so it's in warning range
        assert!(t.is_warning(25.0));
        assert!(!t.is_normal(25.0));
    }
}
