// MQTT message types for channel communication

use crate::libs::alarms::AlarmState;

/// Messages sent to the MQTT monitor thread for publishing
#[derive(Debug, Clone)]
pub enum MqttMessage {
    /// Publish a sensor reading
    PublishSensorReading {
        line: u8,
        temperature: f32,
        is_connected: bool,
        alarm_state: AlarmState,
    },

    /// Publish an alarm state transition event
    PublishAlarmEvent {
        line: u8,
        from_state: AlarmState,
        to_state: AlarmState,
        temperature: f32,
    },

    /// Publish power status
    PublishPowerStatus {
        battery_mv: u16,
        battery_percent: u8,
        vin_mv: u16,
        on_ac_power: bool,
        last_ac_loss_time: Option<u64>,
    },

    /// Publish AC power loss event
    PublishAcLossEvent {
        timestamp: u64,
    },

    /// Publish AC power reconnect event
    PublishAcReconnectEvent {
        timestamp: u64,
    },

    /// Publish network status
    PublishNetworkStatus {
        wifi_connected: bool,
        wifi_signal_dbm: i32,
        ethernet_connected: bool,
    },

    /// Publish system information
    PublishSystemInfo {
        version: String,
        uptime_seconds: u64,
        hostname: String,
    },

    /// Update connection state (internal message)
    SetConnectionState(super::connection::ConnectionState),

    /// Graceful shutdown signal
    Shutdown,
}

/// Commands received from MQTT broker
#[derive(Debug, Clone)]
pub enum MqttCommand {
    /// Set sensor alarm threshold
    SetSensorThreshold {
        line: u8,
        critical_low: f32,
        alarm_low: f32,
        warning_low: f32,
        warning_high: f32,
        alarm_high: f32,
        critical_high: f32,
    },

    /// Get current sensor status
    GetSensorStatus { line: u8 },

    /// Switch display screen
    SetDisplayScreen { screen: String },

    /// Flush storage to disk
    FlushStorage,

    /// Get device information
    GetDeviceInfo,

    /// Restart application
    RestartApplication { reason: String },
}

impl MqttCommand {
    /// Get command name for logging
    pub fn name(&self) -> &'static str {
        match self {
            MqttCommand::SetSensorThreshold { .. } => "set_sensor_threshold",
            MqttCommand::GetSensorStatus { .. } => "get_sensor_status",
            MqttCommand::SetDisplayScreen { .. } => "set_display_screen",
            MqttCommand::FlushStorage => "flush_storage",
            MqttCommand::GetDeviceInfo => "get_device_info",
            MqttCommand::RestartApplication { .. } => "restart_application",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_variants() {
        let msg = MqttMessage::PublishSensorReading {
            line: 0,
            temperature: 36.5,
            is_connected: true,
            alarm_state: AlarmState::Normal,
        };

        match msg {
            MqttMessage::PublishSensorReading { line, .. } => assert_eq!(line, 0),
            _ => panic!("Wrong message type"),
        }
    }

    #[test]
    fn test_command_names() {
        let cmd = MqttCommand::SetSensorThreshold {
            line: 0,
            critical_low: 32.0,
            alarm_low: 34.0,
            warning_low: 35.0,
            warning_high: 39.0,
            alarm_high: 40.0,
            critical_high: 42.0,
        };

        assert_eq!(cmd.name(), "set_sensor_threshold");

        let cmd2 = MqttCommand::FlushStorage;
        assert_eq!(cmd2.name(), "flush_storage");
    }
}
