// MQTT message types for channel communication

use crate::libs::alarms::{AlarmState, AlarmThreshold};
use crate::libs::crypto::UserCertificate;
use crate::libs::sensors::aggregation::AggregationPeriod;
use serde_json::Value;

/// Messages sent to the MQTT monitor thread for publishing
#[derive(Debug, Clone)]
pub enum MqttMessage {
    /// Publish a sensor reading
    PublishSensorReading {
        line: u8,
        name: String,
        temperature: f32,
        is_connected: bool,
        alarm_state: AlarmState,
    },

    /// Publish aggregated sensor data
    PublishAggregatedSensorData {
        period: AggregationPeriod,
        names: [String; 8],
    },

    /// Publish an alarm state transition event
    PublishAlarmEvent {
        line: u8,
        name: String,
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

    /// Publish configuration challenge (preview of changes)
    PublishConfigChallenge {
        challenge_id: String,
        request_id: String,
        signer_id: String,
        expires_at: i64,
        preview: Value, // ChangePreview as JSON
    },

    /// Publish configuration response (success/error)
    PublishConfigResponse {
        challenge_id: String,
        request_id: String,
        status: String, // SUCCESS, ERROR
        applied_at: Option<i64>,
        effective_at: Option<i64>,
        message: String,
    },

    /// Publish sensor configuration data
    PublishSensorConfig {
        sensors: Vec<SensorConfigData>,
    },

    /// Publish interval configuration data
    PublishIntervalConfig {
        sample_interval_ms: u64,
        aggregation_interval_ms: u64,
        report_interval_ms: u64,
    },

    /// Update connection state (internal message)
    SetConnectionState(super::connection::ConnectionState),

    /// Graceful shutdown signal
    Shutdown,
}

/// Sensor configuration data for query response
#[derive(Debug, Clone)]
pub struct SensorConfigData {
    pub line: u8,
    pub name: String,
    pub enabled: bool,
    pub has_override: bool, // true if using per-line thresholds, false if using common defaults
    pub thresholds: AlarmThreshold,
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

    /// Get sensor configuration (all 8 sensors)
    GetSensorConfig,

    /// Set sensor name (signed via ConfigRequest)
    SetSensorName { line: u8, name: String },

    /// Restart application
    RestartApplication { reason: String },

    /// Set sensor intervals (sample, aggregation, report)
    SetInterval {
        sample_interval_ms: u64,
        aggregation_interval_ms: u64,
        report_interval_ms: u64,
    },

    /// Get current sensor intervals
    GetInterval,

    /// Add signer (signed via ConfigRequest)
    AddSigner { signer_data: Value },

    /// Remove signer (signed via ConfigRequest)
    RemoveSigner { signer_id: String },

    /// Update signer (signed via ConfigRequest)
    UpdateSigner {
        signer_id: String,
        changes: Value,
    },

    /// Configuration change request (signed with Ed25519)
    ConfigRequest {
        request_id: String,
        command_type: String,
        params: Value, // Command-specific parameters as JSON
        reason: Option<String>,
        signer_id: String,
        signature: String, // Base64-encoded Ed25519 signature
        timestamp: i64,
        nonce: String,
        certificate: UserCertificate, // User certificate signed by CA
    },

    /// Configuration change confirmation (signed)
    ConfigConfirm {
        challenge_id: String,
        confirmation: String, // APPROVED or REJECTED
        signer_id: String,
        signature: String, // Base64-encoded Ed25519 signature
        timestamp: i64,
        nonce: String,
        certificate: UserCertificate, // User certificate signed by CA
    },
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
            MqttCommand::GetSensorConfig => "get_sensor_config",
            MqttCommand::SetSensorName { .. } => "set_sensor_name",
            MqttCommand::RestartApplication { .. } => "restart_application",
            MqttCommand::SetInterval { .. } => "set_interval",
            MqttCommand::GetInterval => "get_interval",
            MqttCommand::AddSigner { .. } => "add_signer",
            MqttCommand::RemoveSigner { .. } => "remove_signer",
            MqttCommand::UpdateSigner { .. } => "update_signer",
            MqttCommand::ConfigRequest { .. } => "config_request",
            MqttCommand::ConfigConfirm { .. } => "config_confirm",
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
            name: "Test Sensor".to_string(),
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
