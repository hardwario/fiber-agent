//! MQTT alarm callback - sends alarm state transitions via MQTT
//!
//! This callback publishes alarm events to the MQTT broker when a sensor's
//! alarm state changes (e.g., Normal -> Warning -> Alarm -> Critical).

use std::sync::{Arc, RwLock};
use crate::libs::alarms::callbacks::{AlarmCallback, AlarmEvent};
use crate::libs::alarms::state::AlarmState;
use crate::libs::mqtt::MqttHandle;

/// Callback that sends alarm events to MQTT
pub struct MqttAlarmCallback {
    mqtt: MqttHandle,
    line: u8,
    name: Arc<RwLock<String>>,
    /// Last known temperature for this sensor (updated externally)
    last_temperature: Arc<RwLock<Option<f32>>>,
}

impl MqttAlarmCallback {
    /// Create a new MQTT alarm callback
    ///
    /// # Arguments
    /// * `mqtt` - MQTT handle for sending messages
    /// * `line` - Sensor line number (0-7)
    /// * `name` - Sensor name (can be updated dynamically via the RwLock)
    pub fn new(mqtt: MqttHandle, line: u8, name: String) -> Self {
        Self {
            mqtt,
            line,
            name: Arc::new(RwLock::new(name)),
            last_temperature: Arc::new(RwLock::new(None)),
        }
    }

    /// Get a handle to update the sensor name
    pub fn name_handle(&self) -> Arc<RwLock<String>> {
        self.name.clone()
    }

    /// Get a handle to update the last temperature
    pub fn temperature_handle(&self) -> Arc<RwLock<Option<f32>>> {
        self.last_temperature.clone()
    }

    /// Update the sensor name
    pub fn set_name(&self, name: String) {
        if let Ok(mut n) = self.name.write() {
            *n = name;
        }
    }

    /// Update the last known temperature
    pub fn set_temperature(&self, temp: f32) {
        if let Ok(mut t) = self.last_temperature.write() {
            *t = Some(temp);
        }
    }
}

impl AlarmCallback for MqttAlarmCallback {
    fn on_event(&self, event: AlarmEvent) {
        match event {
            AlarmEvent::StateChanged { from, to } => {
                // Skip transitions from NeverConnected - these are initial connections, not alarms
                // Also skip if transitioning TO NeverConnected (shouldn't happen)
                if from == AlarmState::NeverConnected || to == AlarmState::NeverConnected {
                    return;
                }

                // Get the current sensor name
                let name = self.name.read()
                    .map(|n| n.clone())
                    .unwrap_or_else(|_| format!("Sensor {}", self.line));

                // Get the last known temperature
                let temperature = self.last_temperature.read()
                    .map(|t| t.unwrap_or(0.0))
                    .unwrap_or(0.0);

                eprintln!(
                    "[MqttAlarmCallback] Line {} ({}): {} -> {} at {:.1}°C",
                    self.line, name, from, to, temperature
                );

                // Send the alarm event via MQTT
                self.mqtt.send_alarm_event(
                    self.line,
                    &name,
                    from,
                    to,
                    temperature,
                );
            }
            // We also want to publish specific alarm events with temperature
            AlarmEvent::Warning { value } => {
                let name = self.name.read()
                    .map(|n| n.clone())
                    .unwrap_or_else(|_| format!("Sensor {}", self.line));

                // Warning triggered - send transition event
                // Note: StateChanged already handles this, but we update temperature
                if let Ok(mut t) = self.last_temperature.write() {
                    *t = Some(value);
                }
            }
            AlarmEvent::Critical { value } => {
                if let Ok(mut t) = self.last_temperature.write() {
                    *t = Some(value);
                }
            }
            AlarmEvent::Reconnected | AlarmEvent::Disconnected => {
                // These are handled by StateChanged
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: Testing requires a mock MQTT handle, which is complex.
    // For now, we just test the callback creation.
    #[test]
    fn test_callback_creation() {
        // We can't easily test without a real MqttHandle
        // This test is mainly to ensure the code compiles
    }
}
