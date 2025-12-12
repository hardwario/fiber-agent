// MQTT message publishing with JSON formatting

use chrono::{DateTime, Utc};
use rumqttc::{AsyncClient, QoS};
use serde_json::json;

use crate::libs::alarms::AlarmState;
use crate::libs::config::{PublishConfig, QosOverrides};

use super::messages::MqttMessage;
use super::topics::TopicBuilder;

/// Message publisher that formats and publishes MQTT messages
pub struct MqttPublisher {
    client: AsyncClient,
    topics: TopicBuilder,
    qos_overrides: QosOverrides,
}

impl MqttPublisher {
    /// Create a new MQTT publisher
    pub fn new(client: AsyncClient, topics: TopicBuilder, config: &PublishConfig) -> Self {
        Self {
            client,
            topics,
            qos_overrides: config.qos_overrides.clone(),
        }
    }

    /// Get current timestamp as ISO 8601 string
    fn timestamp() -> String {
        let now: DateTime<Utc> = Utc::now();
        now.to_rfc3339()
    }

    /// Convert QoS u8 to rumqttc::QoS
    fn qos_from_u8(qos: u8) -> QoS {
        match qos {
            0 => QoS::AtMostOnce,
            1 => QoS::AtLeastOnce,
            2 => QoS::ExactlyOnce,
            _ => QoS::AtMostOnce,
        }
    }

    /// Publish a message to a topic
    async fn publish(&self, topic: String, payload: String, qos: QoS, retain: bool) -> Result<(), String> {
        self.client
            .publish(topic.clone(), qos, retain, payload.as_bytes())
            .await
            .map_err(|e| format!("Failed to publish to {}: {}", topic, e))
    }

    /// Handle incoming MQTT messages for publishing
    pub async fn handle_message(&self, msg: MqttMessage) -> Result<(), String> {
        match msg {
            MqttMessage::PublishSensorReading {
                line,
                temperature,
                is_connected,
                alarm_state,
            } => {
                self.publish_sensor_reading(line, temperature, is_connected, alarm_state)
                    .await
            }

            MqttMessage::PublishAlarmEvent {
                line,
                from_state,
                to_state,
                temperature,
            } => {
                self.publish_alarm_event(line, from_state, to_state, temperature)
                    .await
            }

            MqttMessage::PublishPowerStatus {
                battery_mv,
                battery_percent,
                vin_mv,
                on_ac_power,
                last_ac_loss_time,
            } => {
                self.publish_power_status(
                    battery_mv,
                    battery_percent,
                    vin_mv,
                    on_ac_power,
                    last_ac_loss_time,
                )
                .await
            }

            MqttMessage::PublishAcLossEvent { timestamp } => {
                self.publish_ac_loss_event(timestamp).await
            }

            MqttMessage::PublishAcReconnectEvent { timestamp } => {
                self.publish_ac_reconnect_event(timestamp).await
            }

            MqttMessage::PublishNetworkStatus {
                wifi_connected,
                wifi_signal_dbm,
                ethernet_connected,
            } => {
                self.publish_network_status(wifi_connected, wifi_signal_dbm, ethernet_connected)
                    .await
            }

            MqttMessage::PublishSystemInfo {
                version,
                uptime_seconds,
                hostname,
            } => {
                self.publish_system_info(&version, uptime_seconds, &hostname)
                    .await
            }

            // Internal messages, not published
            MqttMessage::SetConnectionState(_) | MqttMessage::Shutdown => Ok(()),
        }
    }

    /// Publish sensor reading
    async fn publish_sensor_reading(
        &self,
        line: u8,
        temperature: f32,
        is_connected: bool,
        alarm_state: AlarmState,
    ) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "line": line,
            "temperature_celsius": temperature,
            "is_connected": is_connected,
            "alarm_state": format!("{:?}", alarm_state).to_uppercase(),
        });

        let topic = self.topics.sensor_temperature(line);
        let qos = Self::qos_from_u8(self.qos_overrides.sensor_readings);

        self.publish(topic, payload.to_string(), qos, false).await?;

        // Also publish status (retained)
        let status_payload = json!({
            "timestamp": Self::timestamp(),
            "is_connected": is_connected,
            "alarm_state": format!("{:?}", alarm_state).to_uppercase(),
        });

        let status_topic = self.topics.sensor_status(line);
        let status_qos = Self::qos_from_u8(self.qos_overrides.power_status);

        self.publish(status_topic, status_payload.to_string(), status_qos, true)
            .await
    }

    /// Publish alarm event
    async fn publish_alarm_event(
        &self,
        line: u8,
        from_state: AlarmState,
        to_state: AlarmState,
        temperature: f32,
    ) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "line": line,
            "from_state": format!("{:?}", from_state).to_uppercase(),
            "to_state": format!("{:?}", to_state).to_uppercase(),
            "temperature_celsius": temperature,
            "event_type": "alarm_transition",
        });

        let topic = self.topics.alarms_events();
        let qos = Self::qos_from_u8(self.qos_overrides.alarm_events);

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish power status
    async fn publish_power_status(
        &self,
        battery_mv: u16,
        battery_percent: u8,
        vin_mv: u16,
        on_ac_power: bool,
        last_ac_loss_time: Option<u64>,
    ) -> Result<(), String> {
        let battery_status = if battery_percent < 5 {
            "critical"
        } else if battery_percent < 20 {
            "low"
        } else {
            "normal"
        };

        let payload = json!({
            "timestamp": Self::timestamp(),
            "battery": {
                "voltage_mv": battery_mv,
                "percentage": battery_percent,
                "status": battery_status,
            },
            "ac": {
                "voltage_mv": vin_mv,
                "connected": on_ac_power,
            },
            "last_ac_loss": last_ac_loss_time.map(|t| {
                DateTime::<Utc>::from_timestamp(t as i64, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default()
            }),
        });

        let qos = Self::qos_from_u8(self.qos_overrides.power_status);

        // Publish main status
        let topic = self.topics.power_battery_percentage();
        self.publish(topic, payload.to_string(), qos, false).await?;

        // Publish AC connection status (retained)
        let ac_payload = json!({
            "timestamp": Self::timestamp(),
            "connected": on_ac_power,
        });
        let ac_topic = self.topics.power_ac_connected();
        self.publish(ac_topic, ac_payload.to_string(), qos, true)
            .await
    }

    /// Publish AC loss event
    async fn publish_ac_loss_event(&self, timestamp: u64) -> Result<(), String> {
        let payload = json!({
            "timestamp": DateTime::<Utc>::from_timestamp(timestamp as i64, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(Self::timestamp),
            "event_type": "ac_power_loss",
        });

        let topic = self.topics.power_events_ac_loss();
        let qos = Self::qos_from_u8(self.qos_overrides.power_events);

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish AC reconnect event
    async fn publish_ac_reconnect_event(&self, timestamp: u64) -> Result<(), String> {
        let payload = json!({
            "timestamp": DateTime::<Utc>::from_timestamp(timestamp as i64, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(Self::timestamp),
            "event_type": "ac_power_reconnect",
        });

        let topic = self.topics.power_events_ac_loss();
        let qos = Self::qos_from_u8(self.qos_overrides.power_events);

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish network status
    async fn publish_network_status(
        &self,
        wifi_connected: bool,
        wifi_signal_dbm: i32,
        ethernet_connected: bool,
    ) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "wifi": {
                "connected": wifi_connected,
                "signal_dbm": wifi_signal_dbm,
                "interface": "wlan0",
            },
            "ethernet": {
                "connected": ethernet_connected,
                "interface": "eth0",
            },
        });

        let topic = self.topics.network_wifi_connected();
        let qos = Self::qos_from_u8(self.qos_overrides.network_status);

        self.publish(topic, payload.to_string(), qos, false).await?;

        // Publish WiFi connection status (retained)
        let wifi_payload = json!({
            "timestamp": Self::timestamp(),
            "connected": wifi_connected,
        });
        let wifi_topic = self.topics.network_wifi_connected();
        self.publish(wifi_topic, wifi_payload.to_string(), qos, true)
            .await
    }

    /// Publish system information
    async fn publish_system_info(
        &self,
        version: &str,
        uptime_seconds: u64,
        hostname: &str,
    ) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "hostname": hostname,
            "version": version,
            "uptime_seconds": uptime_seconds,
            "app_name": "FIBER Medical Thermometer",
        });

        let topic = self.topics.info_version();
        let qos = QoS::AtMostOnce;

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish device online status (for Last Will and Testament)
    pub async fn publish_online_status(&self) -> Result<(), String> {
        let payload = json!({
            "status": "online",
            "timestamp": Self::timestamp(),
        });

        let topic = self.topics.status();
        let qos = QoS::AtLeastOnce;

        self.publish(topic, payload.to_string(), qos, true).await
    }

    /// Publish error message
    pub async fn publish_error(&self, command: &str, error: &str, message: &str) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "command": command,
            "error": error,
            "message": message,
        });

        let topic = self.topics.errors();
        let qos = QoS::AtLeastOnce;

        self.publish(topic, payload.to_string(), qos, false).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timestamp_format() {
        let ts = MqttPublisher::timestamp();
        // Should be ISO 8601 format (e.g., "2025-12-11T14:35:22Z")
        assert!(ts.contains('T'));
        assert!(ts.len() > 10);
    }

    #[test]
    fn test_qos_conversion() {
        assert!(matches!(
            MqttPublisher::qos_from_u8(0),
            QoS::AtMostOnce
        ));
        assert!(matches!(
            MqttPublisher::qos_from_u8(1),
            QoS::AtLeastOnce
        ));
        assert!(matches!(
            MqttPublisher::qos_from_u8(2),
            QoS::ExactlyOnce
        ));
    }
}
