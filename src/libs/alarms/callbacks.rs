//! Alarm event callbacks for logging, alerts, and custom actions

use crate::libs::alarms::state::AlarmState;
use std::fmt;

/// Alarm event types
#[derive(Debug, Clone)]
pub enum AlarmEvent {
    /// State transitioned from one state to another
    StateChanged { from: AlarmState, to: AlarmState },
    /// Entered warning state
    Warning { value: f32 },
    /// Entered alarm state
    Alarm { value: f32 },
    /// Entered critical state
    Critical { value: f32 },
    /// Reconnected after disconnection
    Reconnected,
    /// Disconnected/offline
    Disconnected,
}

impl fmt::Display for AlarmEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AlarmEvent::StateChanged { from, to } => {
                write!(f, "State changed: {} → {}", from, to)
            }
            AlarmEvent::Warning { value } => write!(f, "Warning: {:.1}°C", value),
            AlarmEvent::Alarm { value } => write!(f, "Alarm: {:.1}°C", value),
            AlarmEvent::Critical { value } => write!(f, "Critical: {:.1}°C", value),
            AlarmEvent::Reconnected => write!(f, "Sensor reconnected"),
            AlarmEvent::Disconnected => write!(f, "Sensor disconnected"),
        }
    }
}

/// Trait for alarm event callbacks
/// Implement this trait to handle alarm events (logging, buzzing, alerts, etc.)
pub trait AlarmCallback: Send + Sync {
    /// Called when an alarm event occurs
    fn on_event(&self, event: AlarmEvent);
}

/// Built-in callback: logs events to stderr
pub struct LoggingCallback {
    prefix: String,
}

impl LoggingCallback {
    /// Create a logging callback with custom prefix
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
        }
    }

    /// Create a logging callback with default prefix
    pub fn default() -> Self {
        Self {
            prefix: "[Alarm]".to_string(),
        }
    }
}

impl AlarmCallback for LoggingCallback {
    fn on_event(&self, event: AlarmEvent) {
        eprintln!("{} {}", self.prefix, event);
    }
}

/// Built-in callback: filters events and only logs certain ones
pub struct FilteredLoggingCallback {
    prefix: String,
    log_state_changes: bool,
    log_warnings: bool,
    log_alarms: bool,
    log_critical: bool,
    log_reconnect: bool,
    log_disconnect: bool,
}

impl FilteredLoggingCallback {
    /// Create a filtered logging callback with all events enabled
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
            log_state_changes: true,
            log_warnings: true,
            log_alarms: true,
            log_critical: true,
            log_reconnect: true,
            log_disconnect: true,
        }
    }

    /// Enable/disable specific event types
    pub fn with_state_changes(mut self, enabled: bool) -> Self {
        self.log_state_changes = enabled;
        self
    }

    pub fn with_warnings(mut self, enabled: bool) -> Self {
        self.log_warnings = enabled;
        self
    }

    pub fn with_alarms(mut self, enabled: bool) -> Self {
        self.log_alarms = enabled;
        self
    }

    pub fn with_critical(mut self, enabled: bool) -> Self {
        self.log_critical = enabled;
        self
    }

    pub fn with_reconnect(mut self, enabled: bool) -> Self {
        self.log_reconnect = enabled;
        self
    }

    pub fn with_disconnect(mut self, enabled: bool) -> Self {
        self.log_disconnect = enabled;
        self
    }
}

impl AlarmCallback for FilteredLoggingCallback {
    fn on_event(&self, event: AlarmEvent) {
        let should_log = match &event {
            AlarmEvent::StateChanged { .. } => self.log_state_changes,
            AlarmEvent::Warning { .. } => self.log_warnings,
            AlarmEvent::Alarm { .. } => self.log_alarms,
            AlarmEvent::Critical { .. } => self.log_critical,
            AlarmEvent::Reconnected => self.log_reconnect,
            AlarmEvent::Disconnected => self.log_disconnect,
        };

        if should_log {
            eprintln!("{} {}", self.prefix, event);
        }
    }
}

/// Buzzer beep pattern for different alarm types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeepPattern {
    /// Disconnected: steady beep pattern (100ms on, 100ms off, repeating)
    DisconnectedBeep,
    /// Critical: urgent beep pattern (200ms on, 100ms off, repeating)
    CriticalBeep,
}

/// Built-in callback: triggers buzzer with different patterns for different alarms (logging only)
pub struct BuzzerCallback {
    prefix: String,
}

impl BuzzerCallback {
    /// Create a buzzer callback with custom prefix (for logging)
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
        }
    }

    /// Create a buzzer callback with default prefix
    pub fn default() -> Self {
        Self {
            prefix: "[Buzzer]".to_string(),
        }
    }
}

impl AlarmCallback for BuzzerCallback {
    fn on_event(&self, event: AlarmEvent) {
        match &event {
            AlarmEvent::Disconnected => {
                eprintln!("{} Disconnected - should start beep pattern {:?}", self.prefix, BeepPattern::DisconnectedBeep);
            }
            AlarmEvent::Critical { .. } => {
                eprintln!("{} Critical alarm - should start beep pattern {:?}", self.prefix, BeepPattern::CriticalBeep);
            }
            AlarmEvent::Reconnected => {
                eprintln!("{} Reconnected - should stop buzzer", self.prefix);
            }
            _ => {}
        }
    }
}

/// Built-in callback: tracks buzzer state for hardware control
/// This callback doesn't directly control hardware - instead it tracks state that the monitor loop uses
pub struct BuzzerStateCallback {
    prefix: String,
}

impl BuzzerStateCallback {
    /// Create a buzzer state callback with custom prefix
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
        }
    }
}

impl AlarmCallback for BuzzerStateCallback {
    fn on_event(&self, event: AlarmEvent) {
        // This callback is used to track state changes
        // The actual buzzer control happens in the monitor loop
        match &event {
            AlarmEvent::Disconnected => {
                eprintln!("{} [STATE] Sensor disconnected - buzzer should beep (pattern: 100ms on/off)", self.prefix);
            }
            AlarmEvent::Critical { value } => {
                eprintln!("{} [STATE] Critical alarm at {:.1}°C - buzzer should beep (pattern: 200ms on/100ms off)", self.prefix, value);
            }
            AlarmEvent::Reconnected => {
                eprintln!("{} [STATE] Sensor reconnected - buzzer should stop", self.prefix);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alarm_event_display() {
        let event = AlarmEvent::Warning { value: 37.5 };
        assert_eq!(event.to_string(), "Warning: 37.5°C");

        let event = AlarmEvent::Critical { value: 41.0 };
        assert_eq!(event.to_string(), "Critical: 41.0°C");

        let event = AlarmEvent::Reconnected;
        assert_eq!(event.to_string(), "Sensor reconnected");
    }

    #[test]
    fn test_logging_callback_creation() {
        let cb = LoggingCallback::new("[Sensor 1]");
        assert_eq!(cb.prefix, "[Sensor 1]");

        let cb = LoggingCallback::default();
        assert_eq!(cb.prefix, "[Alarm]");
    }

    #[test]
    fn test_filtered_logging_callback() {
        let cb = FilteredLoggingCallback::new("[Test]")
            .with_warnings(false)
            .with_alarms(true);
        assert!(!cb.log_warnings);
        assert!(cb.log_alarms);
    }

    #[test]
    fn test_buzzer_callback_creation() {
        let cb = BuzzerCallback::new("[Buzzer]");
        assert_eq!(cb.prefix, "[Buzzer]");

        let cb = BuzzerCallback::default();
        assert_eq!(cb.prefix, "[Buzzer]");
    }

    #[test]
    fn test_beep_pattern_display() {
        assert_eq!(format!("{:?}", BeepPattern::DisconnectedBeep), "DisconnectedBeep");
        assert_eq!(format!("{:?}", BeepPattern::CriticalBeep), "CriticalBeep");
    }

    #[test]
    fn test_buzzer_callback_handles_disconnect() {
        let cb = BuzzerCallback::default();
        // Should not panic
        cb.on_event(AlarmEvent::Disconnected);
    }

    #[test]
    fn test_buzzer_callback_handles_critical() {
        let cb = BuzzerCallback::default();
        // Should not panic
        cb.on_event(AlarmEvent::Critical { value: 41.0 });
    }
}
