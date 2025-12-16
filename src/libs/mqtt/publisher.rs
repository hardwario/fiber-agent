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
                name,
                temperature,
                is_connected,
                alarm_state,
            } => {
                self.publish_sensor_reading(line, &name, temperature, is_connected, alarm_state)
                    .await
            }

            MqttMessage::PublishAlarmEvent {
                line,
                name,
                from_state,
                to_state,
                temperature,
            } => {
                self.publish_alarm_event(line, &name, from_state, to_state, temperature)
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

            MqttMessage::PublishAggregatedSensorData { period, names } => {
                self.publish_aggregated_sensor_data(period, &names).await
            }

            MqttMessage::PublishConfigChallenge {
                challenge_id,
                request_id,
                signer_id,
                expires_at,
                preview,
            } => {
                self.publish_config_challenge(&challenge_id, &request_id, &signer_id, expires_at, preview)
                    .await
            }

            MqttMessage::PublishConfigResponse {
                challenge_id,
                request_id,
                status,
                applied_at,
                effective_at,
                message,
            } => {
                self.publish_config_response(
                    &challenge_id,
                    &request_id,
                    &status,
                    applied_at,
                    effective_at,
                    &message,
                )
                .await
            }

            MqttMessage::PublishSensorConfig { sensors } => {
                self.publish_sensor_config(sensors).await
            }

            MqttMessage::PublishIntervalConfig {
                sample_interval_ms,
                aggregation_interval_ms,
                report_interval_ms,
            } => {
                self.publish_interval_config(sample_interval_ms, aggregation_interval_ms, report_interval_ms)
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
        name: &str,
        temperature: f32,
        is_connected: bool,
        alarm_state: AlarmState,
    ) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "line": line,
            "name": name,
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
            "name": name,
            "is_connected": is_connected,
            "alarm_state": format!("{:?}", alarm_state).to_uppercase(),
        });

        let status_topic = self.topics.sensor_status(line);
        let status_qos = Self::qos_from_u8(self.qos_overrides.power_status);

        self.publish(status_topic, status_payload.to_string(), status_qos, true)
            .await
    }

    /// Publish aggregated sensor data for a completed period
    async fn publish_aggregated_sensor_data(
        &self,
        period: crate::libs::sensors::aggregation::AggregationPeriod,
        names: &[String; 8],
    ) -> Result<(), String> {
        // Build JSON payload with all 8 sensors
        let sensors_data: Vec<serde_json::Value> = period.sensors.iter()
            .map(|sensor| {
                // Only include valid sensor data
                let temp_data = if sensor.sample_count > 0 {
                    json!({
                        "min_celsius": sensor.min_temp_celsius,
                        "max_celsius": sensor.max_temp_celsius,
                        "avg_celsius": sensor.avg_temp_celsius,
                    })
                } else {
                    serde_json::Value::Null
                };

                // Get sensor name
                let name = &names[sensor.line as usize];

                json!({
                    "line": sensor.line,
                    "name": name,
                    "sample_count": sensor.sample_count,
                    "disconnected_count": sensor.disconnected_count,
                    "temperature": temp_data,
                    "alarm_counts": {
                        "normal": sensor.alarm_counts.normal,
                        "warning": sensor.alarm_counts.warning,
                        "alarm": sensor.alarm_counts.alarm,
                        "critical": sensor.alarm_counts.critical,
                        "disconnected": sensor.alarm_counts.disconnected,
                        "reconnecting": sensor.alarm_counts.reconnecting,
                    },
                    "dominant_alarm_state": format!("{:?}", sensor.dominant_alarm_state()).to_uppercase(),
                })
            })
            .collect();

        let payload = json!({
            "timestamp": Self::timestamp(),
            "period_start_ts": period.period_start_ts,
            "period_end_ts": period.period_end_ts,
            "duration_sec": period.period_end_ts - period.period_start_ts,
            "sensors": sensors_data,
        });

        let topic = self.topics.sensors_aggregated();
        let qos = Self::qos_from_u8(self.qos_overrides.sensor_readings);

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish alarm event
    async fn publish_alarm_event(
        &self,
        line: u8,
        name: &str,
        from_state: AlarmState,
        to_state: AlarmState,
        temperature: f32,
    ) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "line": line,
            "name": name,
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

        // Publish power status (includes battery and AC info)
        let topic = self.topics.power_battery_percentage();
        self.publish(topic, payload.to_string(), qos, false).await
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
            "has_internet": wifi_connected || ethernet_connected,
        });

        let topic = self.topics.network_status();
        let qos = Self::qos_from_u8(self.qos_overrides.network_status);

        self.publish(topic, payload.to_string(), qos, true).await
    }

    /// Publish system information
    async fn publish_system_info(
        &self,
        version: &str,
        uptime_seconds: u64,
        hostname: &str,
    ) -> Result<(), String> {
        // Format uptime in human-readable form
        let days = uptime_seconds / 86400;
        let hours = (uptime_seconds % 86400) / 3600;
        let minutes = (uptime_seconds % 3600) / 60;
        let secs = uptime_seconds % 60;
        let uptime_human = if days > 0 {
            format!("{}d {}h {}m {}s", days, hours, minutes, secs)
        } else if hours > 0 {
            format!("{}h {}m {}s", hours, minutes, secs)
        } else if minutes > 0 {
            format!("{}m {}s", minutes, secs)
        } else {
            format!("{}s", secs)
        };

        let payload = json!({
            "timestamp": Self::timestamp(),
            "hostname": hostname,
            "firmware_version": version,
            "uptime_seconds": uptime_seconds,
            "uptime_human": uptime_human,
            "app_name": "FIBER Medical Thermometer",
        });

        let topic = self.topics.system_info();
        let qos = QoS::AtLeastOnce;

        self.publish(topic, payload.to_string(), qos, true).await
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

    /// Publish configuration challenge (preview of changes)
    pub async fn publish_config_challenge(
        &self,
        challenge_id: &str,
        request_id: &str,
        signer_id: &str,
        expires_at: i64,
        preview: serde_json::Value,
    ) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "challenge_id": challenge_id,
            "request_id": request_id,
            "signer_id": signer_id,
            "expires_at": expires_at,
            "expires_at_iso": DateTime::<Utc>::from_timestamp(expires_at, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default(),
            "preview": preview,
            "status": "awaiting_confirmation",
        });

        let topic = self.topics.config_challenge();
        let qos = QoS::ExactlyOnce; // QoS 2 for critical configuration messages

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish configuration response (success/error after applying)
    pub async fn publish_config_response(
        &self,
        challenge_id: &str,
        request_id: &str,
        status: &str,
        applied_at: Option<i64>,
        effective_at: Option<i64>,
        message: &str,
    ) -> Result<(), String> {
        let mut payload = json!({
            "timestamp": Self::timestamp(),
            "challenge_id": challenge_id,
            "request_id": request_id,
            "status": status,
            "message": message,
        });

        // Add applied_at if present
        if let Some(ts) = applied_at {
            payload["applied_at"] = json!(ts);
            payload["applied_at_iso"] = json!(
                DateTime::<Utc>::from_timestamp(ts, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default()
            );
        }

        // Add effective_at if present
        if let Some(ts) = effective_at {
            payload["effective_at"] = json!(ts);
            payload["effective_at_iso"] = json!(
                DateTime::<Utc>::from_timestamp(ts, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default()
            );
        }

        let topic = self.topics.config_response();
        let qos = QoS::ExactlyOnce; // QoS 2 for critical configuration messages

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish sensor configuration data (all 8 sensors)
    pub async fn publish_sensor_config(
        &self,
        sensors: Vec<super::messages::SensorConfigData>,
    ) -> Result<(), String> {
        let sensors_data: Vec<serde_json::Value> = sensors
            .iter()
            .map(|sensor| {
                json!({
                    "line": sensor.line,
                    "name": sensor.name,
                    "enabled": sensor.enabled,
                    "has_override": sensor.has_override,
                    "thresholds": {
                        "critical_low_celsius": sensor.thresholds.critical_low_celsius,
                        "low_alarm_celsius": sensor.thresholds.low_alarm_celsius,
                        "warning_low_celsius": sensor.thresholds.warning_low_celsius,
                        "warning_high_celsius": sensor.thresholds.warning_high_celsius,
                        "high_alarm_celsius": sensor.thresholds.high_alarm_celsius,
                        "critical_high_celsius": sensor.thresholds.critical_high_celsius,
                    },
                })
            })
            .collect();

        let payload = json!({
            "timestamp": Self::timestamp(),
            "sensors": sensors_data,
        });

        let topic = self.topics.responses_sensor_config();
        let qos = QoS::AtLeastOnce; // QoS 1 for query responses

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish interval configuration data
    pub async fn publish_interval_config(
        &self,
        sample_interval_ms: u64,
        aggregation_interval_ms: u64,
        report_interval_ms: u64,
    ) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "intervals": {
                "sample_interval_ms": sample_interval_ms,
                "aggregation_interval_ms": aggregation_interval_ms,
                "report_interval_ms": report_interval_ms,
            },
        });

        let topic = self.topics.responses_interval_config();
        let qos = QoS::AtLeastOnce; // QoS 1 for query responses

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
