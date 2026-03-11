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

            MqttMessage::PublishSystemStatus {
                hostname,
                device_label,
                version,
                uptime_seconds,
                battery_mv,
                battery_percent,
                vin_mv,
                on_dc_power,
                last_dc_loss_time,
                wifi_connected,
                wifi_signal_dbm,
                wifi_ip,
                ethernet_connected,
                ethernet_ip,
                storage_total_bytes,
                storage_available_bytes,
                storage_used_percent,
                lorawan_gateway_present,
                lorawan_concentratord_running,
                lorawan_chirpstack_running,
                lorawan_sensor_count,
            } => {
                self.publish_system_status(
                    &hostname,
                    &device_label,
                    &version,
                    uptime_seconds,
                    battery_mv,
                    battery_percent,
                    vin_mv,
                    on_dc_power,
                    last_dc_loss_time,
                    wifi_connected,
                    wifi_signal_dbm,
                    wifi_ip,
                    ethernet_connected,
                    ethernet_ip,
                    storage_total_bytes,
                    storage_available_bytes,
                    storage_used_percent,
                    lorawan_gateway_present,
                    lorawan_concentratord_running,
                    lorawan_chirpstack_running,
                    lorawan_sensor_count,
                )
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

            MqttMessage::PublishConfigState {
                led_brightness,
                screen_brightness,
                buzzer_volume,
                system_info_interval_s,
                device_label,
                sensors,
                sample_interval_ms,
                aggregation_interval_ms,
                report_interval_ms,
            } => {
                self.publish_config_state(
                    led_brightness,
                    screen_brightness,
                    buzzer_volume,
                    system_info_interval_s,
                    &device_label,
                    sensors,
                    sample_interval_ms,
                    aggregation_interval_ms,
                    report_interval_ms,
                )
                .await
            }

            MqttMessage::PublishLoRaWANSensorData { sensors } => {
                self.publish_lorawan_sensors(sensors).await
            }

            MqttMessage::PublishPairingResponse(response) => {
                self.publish_pairing_response(&response).await
            }

            MqttMessage::PublishPairingError(error) => {
                self.publish_pairing_error(&error).await
            }

            // Internal messages, not published
            MqttMessage::SetConnectionState(_) | MqttMessage::Shutdown => Ok(()),
        }
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
                        "critical": sensor.alarm_counts.critical,
                        "disconnected": sensor.alarm_counts.disconnected,
                        "reconnecting": sensor.alarm_counts.reconnecting,
                    },
                    "dominant_alarm_state": format!("{:?}", sensor.dominant_alarm_state()).to_uppercase(),
                    "alarm_triggered_at": sensor.alarm_triggered_at.map(|ts| {
                        DateTime::<Utc>::from_timestamp(ts as i64, 0)
                            .map(|dt| dt.to_rfc3339())
                            .unwrap_or_default()
                    }),
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

    /// Publish combined system status (power, network, storage, uptime, lorawan)
    #[allow(clippy::too_many_arguments)]
    async fn publish_system_status(
        &self,
        hostname: &str,
        device_label: &str,
        version: &str,
        uptime_seconds: u64,
        battery_mv: u16,
        battery_percent: u8,
        vin_mv: u16,
        on_dc_power: bool,
        last_dc_loss_time: Option<u64>,
        wifi_connected: bool,
        wifi_signal_dbm: i32,
        wifi_ip: Option<String>,
        ethernet_connected: bool,
        ethernet_ip: Option<String>,
        storage_total_bytes: u64,
        storage_available_bytes: u64,
        storage_used_percent: u8,
        lorawan_gateway_present: bool,
        lorawan_concentratord_running: bool,
        lorawan_chirpstack_running: bool,
        lorawan_sensor_count: usize,
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

        // Determine battery status
        let battery_status = if battery_percent < 5 {
            "critical"
        } else if battery_percent < 20 {
            "low"
        } else {
            "normal"
        };

        // Format last AC loss timestamp
        let last_dc_loss = last_dc_loss_time.and_then(|ts| {
            DateTime::<Utc>::from_timestamp(ts as i64, 0)
                .map(|dt| dt.to_rfc3339())
        });

        let payload = json!({
            "timestamp": Self::timestamp(),
            "hostname": hostname,
            "device_label": device_label,
            "firmware_version": version,
            "uptime_seconds": uptime_seconds,
            "uptime_human": uptime_human,
            "power": {
                "battery": {
                    "voltage_mv": battery_mv,
                    "percentage": battery_percent,
                    "status": battery_status,
                },
                "dc": {
                    "voltage_mv": vin_mv,
                    "connected": on_dc_power,
                },
                "last_dc_loss": last_dc_loss,
            },
            "network": {
                "wifi": {
                    "connected": wifi_connected,
                    "signal_dbm": wifi_signal_dbm,
                    "ip": wifi_ip,
                },
                "ethernet": {
                    "connected": ethernet_connected,
                    "ip": ethernet_ip,
                },
                "has_internet": wifi_connected || ethernet_connected,
            },
            "storage": {
                "data_partition": {
                    "total_bytes": storage_total_bytes,
                    "available_bytes": storage_available_bytes,
                    "used_percent": storage_used_percent,
                },
            },
            "lorawan": {
                "gateway_present": lorawan_gateway_present,
                "concentratord_running": lorawan_concentratord_running,
                "chirpstack_running": lorawan_chirpstack_running,
                "sensor_count": lorawan_sensor_count,
            },
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

    /// Publish full device config state to config/state topic
    #[allow(clippy::too_many_arguments)]
    pub async fn publish_config_state(
        &self,
        led_brightness: u8,
        screen_brightness: u8,
        buzzer_volume: u8,
        system_info_interval_s: u64,
        device_label: &str,
        sensors: Vec<super::messages::SensorConfigData>,
        sample_interval_ms: u64,
        aggregation_interval_ms: u64,
        report_interval_ms: u64,
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
            "led_brightness": led_brightness,
            "screen_brightness": screen_brightness,
            "buzzer_volume": buzzer_volume,
            "system_info_interval_s": system_info_interval_s,
            "device_label": device_label,
            "sensors": sensors_data,
            "intervals": {
                "sample_interval_ms": sample_interval_ms,
                "aggregation_interval_ms": aggregation_interval_ms,
                "report_interval_ms": report_interval_ms,
            },
        });

        let topic = self.topics.config_state();
        let qos = QoS::AtLeastOnce;

        self.publish(topic, payload.to_string(), qos, true).await
    }

    /// Publish LoRaWAN sensor data
    async fn publish_lorawan_sensors(
        &self,
        sensors: Vec<super::messages::LoRaWANSensorPayload>,
    ) -> Result<(), String> {
        let sensors_data: Vec<serde_json::Value> = sensors
            .iter()
            .map(|s| {
                json!({
                    "dev_eui": s.dev_eui,
                    "name": s.name,
                    "temperature": s.temperature,
                    "humidity": s.humidity,
                    "voltage": s.voltage,
                    "ext_temperature_1": s.ext_temperature_1,
                    "ext_temperature_2": s.ext_temperature_2,
                    "illuminance": s.illuminance,
                    "motion_count": s.motion_count,
                    "orientation": s.orientation,
                    "rssi": s.rssi,
                    "snr": s.snr,
                    "last_seen": s.last_seen,
                    "alarm_state": s.alarm_state,
                })
            })
            .collect();

        let payload = json!({
            "timestamp": Self::timestamp(),
            "sensors": sensors_data,
        });

        let topic = self.topics.lorawan_sensors();
        let qos = Self::qos_from_u8(self.qos_overrides.sensor_readings);

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish pairing response (success)
    pub async fn publish_pairing_response(
        &self,
        response: &crate::libs::pairing::messages::PairingResponse,
    ) -> Result<(), String> {
        let payload = serde_json::to_string(&response)
            .map_err(|e| format!("Failed to serialize pairing response: {}", e))?;

        let topic = self.topics.pair_response();
        let qos = QoS::ExactlyOnce; // QoS 2 for pairing (critical)

        self.publish(topic, payload, qos, false).await
    }

    /// Publish pairing error
    pub async fn publish_pairing_error(
        &self,
        error: &crate::libs::pairing::messages::PairingError,
    ) -> Result<(), String> {
        let payload = serde_json::to_string(&error)
            .map_err(|e| format!("Failed to serialize pairing error: {}", e))?;

        let topic = self.topics.pair_response();
        let qos = QoS::ExactlyOnce; // QoS 2 for pairing (critical)

        self.publish(topic, payload, qos, false).await
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
