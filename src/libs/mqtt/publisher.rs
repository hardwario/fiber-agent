// MQTT message publishing with JSON formatting

use chrono::{DateTime, Utc};
use rumqttc::{AsyncClient, QoS};
use serde_json::json;

use crate::libs::alarms::AlarmState;
use crate::libs::config::{PublishConfig, QosOverrides};

use super::messages::MqttMessage;
use super::topics::TopicBuilder;

/// How many of a sensor's most-recent buffered events to include in the
/// periodic combined snapshot at the least aggressive rung of
/// `LORAWAN_SENSORS_CAP_LADDER`. This only bounds the periodic broadcast
/// snapshot — the in-memory backlog itself
/// (`lorawan::state::MAX_RECENT_EVENTS` = 32) is unaffected — and
/// alarm-type events are preferentially retained over routine ones when a
/// tighter rung must trim further (see `select_events`), since `events[]`
/// is the only channel carrying Node native alarms to the viewer's
/// Alarms/email pipeline.
const LORAWAN_SENSORS_EVENTS_CAP: usize = 8;

/// How many gateways' reception detail to keep per sensor at the least
/// aggressive rung. The top-level `rssi`/`snr` fields already carry the
/// strongest receiver's numbers; this only adds secondary detail.
const LORAWAN_SENSORS_GATEWAYS_CAP: usize = 5;

/// Safe byte budget for the combined `lorawan/sensors` publish, comfortably
/// under the 20480-byte outgoing-packet ceiling explicitly configured in
/// `mqtt/monitor.rs::create_mqtt_options`, with headroom for the topic name
/// and MQTT framing overhead. A single sensor with a full 32-event backlog
/// and several gateways was already ~14KB before this cap existed — see the
/// commit this constant was introduced in.
const LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES: usize = 8192;

/// The actual wire ceiling this payload must never exceed, matching
/// `MQTT_MAX_PACKET_SIZE_BYTES` in `mqtt/monitor.rs::create_mqtt_options`
/// exactly (kept as a separate constant, not shared across the module
/// boundary, since publisher.rs has no dependency on monitor.rs — if you
/// change one, change the other). `LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES`
/// above is a *comfortable target* the ladder tries to stay under; this is
/// the hard limit only the minimal-fields last resort is judged against,
/// since a pathologically large fleet may legitimately need more than the
/// comfortable budget just for `dev_eui`+safety-fields, and that's fine as
/// long as it still fits on the wire at all.
const LORAWAN_SENSORS_HARD_CEILING_BYTES: usize = 20 * 1024;

/// Ladder of (events_cap, gateways_cap) pairs tried in order, most detailed
/// first, until the combined payload fits under
/// `LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES`. Gateways (pure diagnostics)
/// degrade before events (alarm-bearing) — see `build_lorawan_sensors_payload`
/// for what happens if every rung still exceeds budget.
const LORAWAN_SENSORS_CAP_LADDER: &[(usize, usize)] = &[
    (LORAWAN_SENSORS_EVENTS_CAP, LORAWAN_SENSORS_GATEWAYS_CAP),
    (LORAWAN_SENSORS_EVENTS_CAP, 1),
    (4, 1),
    (2, 1),
    (1, 0),
    (0, 0),
];

/// Selects up to `cap` of a sensor's events for the periodic snapshot,
/// preferring `event_type == "alarm"` entries (the only ones the viewer's
/// Alarms/email pipeline consumes) over routine ones
/// (`boot`/`orientation`/`hall_active`) when the buffer must be trimmed.
/// `events` is chronological, oldest-first (see `state.rs`'s
/// push_back/pop_front ring buffer) — this restores that order in the
/// output, with alarms filled in newest-first before routine events take
/// any remaining slots.
fn select_events(
    events: &[crate::libs::lorawan::chirpstack::NodeEvent],
    cap: usize,
) -> Vec<crate::libs::lorawan::chirpstack::NodeEvent> {
    if cap == 0 || events.is_empty() {
        return Vec::new();
    }
    if events.len() <= cap {
        return events.to_vec();
    }

    let mut alarm_idx: Vec<usize> = Vec::new();
    let mut other_idx: Vec<usize> = Vec::new();
    for (i, e) in events.iter().enumerate() {
        if e.event_type == "alarm" {
            alarm_idx.push(i);
        } else {
            other_idx.push(i);
        }
    }
    // Higher index = more recent; take newest of each group first.
    alarm_idx.reverse();
    other_idx.reverse();

    let mut selected: Vec<usize> = alarm_idx.into_iter().take(cap).collect();
    if selected.len() < cap {
        let remaining = cap - selected.len();
        selected.extend(other_idx.into_iter().take(remaining));
    }

    selected.sort_unstable();
    selected.into_iter().map(|i| events[i].clone()).collect()
}

/// Serializes one sensor's data, capping its `events`/`gateways` detail. The
/// caps are parameters (rather than always using the module constants) so
/// `build_lorawan_sensors_payload` can try progressively more aggressive
/// rungs of `LORAWAN_SENSORS_CAP_LADDER` if the result comes out over budget.
fn lorawan_sensor_json(
    s: &super::messages::LoRaWANSensorPayload,
    events_cap: usize,
    gateways_cap: usize,
) -> serde_json::Value {
    let events = select_events(&s.events, events_cap);

    let mut gateways = s.gateways.clone();
    gateways.sort_by(|a, b| b.rssi.unwrap_or(i32::MIN).cmp(&a.rssi.unwrap_or(i32::MIN)));
    gateways.truncate(gateways_cap);

    json!({
        "dev_eui": s.dev_eui,
        "name": s.name,
        "serial_number": s.serial_number,
        "location": s.location,
        "fields": s.fields,
        "field_alarm_states": s.field_alarm_states,
        "field_thresholds": s.field_thresholds,
        "counters": s.counters,
        "events": events,
        // Additive per-gateway reception detail. `rssi`/`snr` stay the
        // strongest receiver so existing consumers are unaffected;
        // `gateways[]` is what tells you WHICH gateway heard the frame.
        "gateways": gateways,
        // `fcnt` is lifted out of `counters` to a top-level field so a
        // consumer does not have to know it lives under a counter name.
        "fcnt": s.counters.get("fCnt").copied(),
        "dr": s.dr,
        "downlink_gateway_id": s.downlink_gateway_id,
        "rssi": s.rssi,
        "snr": s.snr,
        "last_seen": s.last_seen,
        "alarm_state": s.alarm_state,
    })
}

/// Absolute-last-resort fields when even the most aggressive ladder rung
/// still exceeds budget (pathologically many sensors). The sensor *list*
/// itself must never shrink — the viewer's `sync_lorawan_sensors`
/// reconciliation deletes any Node missing from a snapshot — so this keeps
/// only what's needed for that reconciliation plus current alarm-relevant
/// state, dropping every diagnostic/detail field.
fn lorawan_sensor_json_minimal(s: &super::messages::LoRaWANSensorPayload) -> serde_json::Value {
    json!({
        "dev_eui": s.dev_eui,
        "name": s.name,
        "fields": s.fields,
        "field_alarm_states": s.field_alarm_states,
        "field_thresholds": s.field_thresholds,
        "counters": s.counters,
        "alarm_state": s.alarm_state,
        "last_seen": s.last_seen,
    })
}

/// Builds the `lorawan/sensors` payload, guaranteed to stay under
/// `LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES` in all but pathological cases.
/// Tries each rung of `LORAWAN_SENSORS_CAP_LADDER` in order (most detail
/// first); if the combined result is still too big at every rung — e.g. a
/// gateway with dozens of paired Nodes — falls back to
/// `lorawan_sensor_json_minimal`, which always preserves every sensor and
/// its safety-relevant fields (`dev_eui`, `fields`, `field_alarm_states`,
/// `field_thresholds`, `counters`, `alarm_state`) even though it drops
/// events/gateways/diagnostics entirely. This minimal fallback is only
/// held to `LORAWAN_SENSORS_HARD_CEILING_BYTES` — the real wire limit — not
/// the tighter comfortable budget the ladder itself targets.
fn build_lorawan_sensors_payload(
    sensors: &[super::messages::LoRaWANSensorPayload],
    timestamp: &str,
) -> String {
    for &(events_cap, gateways_cap) in LORAWAN_SENSORS_CAP_LADDER {
        let sensors_data: Vec<_> = sensors
            .iter()
            .map(|s| lorawan_sensor_json(s, events_cap, gateways_cap))
            .collect();
        let payload = json!({ "timestamp": timestamp, "sensors": sensors_data }).to_string();
        if payload.len() <= LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES {
            return payload;
        }
        eprintln!(
            "[MQTT Publisher] lorawan/sensors payload {} bytes exceeds budget {} at events_cap={} gateways_cap={} ({} sensors); trying a more aggressive cap",
            payload.len(),
            LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES,
            events_cap,
            gateways_cap,
            sensors.len()
        );
    }

    let sensors_data: Vec<_> = sensors.iter().map(lorawan_sensor_json_minimal).collect();
    let payload = json!({ "timestamp": timestamp, "sensors": sensors_data }).to_string();
    if payload.len() > LORAWAN_SENSORS_HARD_CEILING_BYTES {
        let dev_euis: Vec<&str> = sensors.iter().map(|s| s.dev_eui.as_str()).collect();
        eprintln!(
            "[MQTT Publisher] lorawan/sensors payload still {} bytes after minimal-fields fallback (hard ceiling {}, {} sensors: {:?}); publishing anyway",
            payload.len(),
            LORAWAN_SENSORS_HARD_CEILING_BYTES,
            sensors.len(),
            dev_euis
        );
    }
    payload
}

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
    pub(crate) fn timestamp() -> String {
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
    async fn publish(
        &self,
        topic: String,
        payload: String,
        qos: QoS,
        retain: bool,
    ) -> Result<(), String> {
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

            MqttMessage::PublishSystemAlarmEvent {
                alarm_type,
                name,
                from_state,
                to_state,
                message,
            } => {
                self.publish_system_alarm_event(
                    &alarm_type,
                    &name,
                    &from_state,
                    &to_state,
                    &message,
                )
                .await
            }

            MqttMessage::PublishAccelerometerEvent {
                x_g,
                y_g,
                z_g,
                position,
            } => {
                self.publish_accelerometer_event(x_g, y_g, z_g, position)
                    .await
            }

            MqttMessage::PublishStandbyState {
                standby,
                reason,
                requested_by,
                entered_at,
                vin_mv,
            } => {
                self.publish_standby_state(
                    standby,
                    &reason,
                    &requested_by,
                    entered_at.as_deref(),
                    vin_mv,
                )
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

            MqttMessage::PublishAggregatedSensorData {
                period,
                names,
                locations,
            } => {
                self.publish_aggregated_sensor_data(period, &names, &locations)
                    .await
            }

            MqttMessage::PublishConfigChallenge {
                challenge_id,
                request_id,
                signer_id,
                expires_at,
                preview,
            } => {
                self.publish_config_challenge(
                    &challenge_id,
                    &request_id,
                    &signer_id,
                    expires_at,
                    preview,
                )
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
                self.publish_interval_config(
                    sample_interval_ms,
                    aggregation_interval_ms,
                    report_interval_ms,
                )
                .await
            }

            MqttMessage::PublishConfigState {
                led_brightness,
                screen_brightness,
                screen_timeout_secs,
                buzzer_volume,
                system_info_interval_s,
                device_label,
                sensors,
                lorawan_sensors,
                sample_interval_ms,
                aggregation_interval_ms,
                report_interval_ms,
                beacon_enabled,
                beacon_auto_provision,
                beacon_auto_discover,
            } => {
                self.publish_config_state(
                    led_brightness,
                    screen_brightness,
                    screen_timeout_secs,
                    buzzer_volume,
                    system_info_interval_s,
                    &device_label,
                    sensors,
                    lorawan_sensors,
                    sample_interval_ms,
                    aggregation_interval_ms,
                    report_interval_ms,
                    beacon_enabled,
                    beacon_auto_provision,
                    beacon_auto_discover,
                )
                .await
            }

            MqttMessage::PublishLoRaWANSensorData { sensors } => {
                self.publish_lorawan_sensors(sensors).await
            }

            MqttMessage::PublishLoRaWANGatewayData { gateways } => {
                self.publish_lorawan_gateways(gateways).await
            }

            MqttMessage::PublishNodeConfig {
                dev_eui,
                config,
                page_index,
                page_count,
                last_seq,
                last_result,
            } => {
                self.publish_node_config(
                    dev_eui,
                    config,
                    page_index,
                    page_count,
                    last_seq,
                    last_result,
                )
                .await
            }

            MqttMessage::PublishNodeFullConfig {
                dev_eui,
                config,
                page_count,
                last_seq,
                read_status,
                missing,
            } => {
                self.publish_node_full_config(
                    dev_eui,
                    config,
                    page_count,
                    last_seq,
                    read_status,
                    missing,
                )
                .await
            }

            MqttMessage::PublishNodeInfo { dev_eui, info } => {
                self.publish_node_info(dev_eui, info).await
            }

            MqttMessage::ClearNodeInfo { dev_eui } => self.clear_node_info(&dev_eui).await,

            MqttMessage::PublishNodeCommandResult {
                dev_eui,
                command,
                seq,
                result,
                expect,
                detail,
                fault_key,
            } => {
                self.publish_node_command_result(
                    dev_eui, command, seq, result, expect, detail, fault_key,
                )
                .await
            }

            MqttMessage::PublishNodeHistory {
                dev_eui,
                frame_index,
                frame_count,
                records,
            } => {
                self.publish_node_history(dev_eui, frame_index, frame_count, records)
                    .await
            }

            MqttMessage::PublishBeaconSensorData { tags } => {
                self.publish_beacon_sensors(tags).await
            }

            MqttMessage::PublishBeaconDetectResult {
                mac,
                is_en12830,
                status,
            } => {
                self.publish_beacon_detect_result(&mac, is_en12830, &status)
                    .await
            }

            MqttMessage::PublishPairingResponse(response) => {
                self.publish_pairing_response(&response).await
            }

            MqttMessage::PublishPairingError(error) => self.publish_pairing_error(&error).await,

            // Internal messages, not published
            MqttMessage::SetConnectionState(_) | MqttMessage::Shutdown => Ok(()),
        }
    }

    /// Publish aggregated sensor data for a completed period
    async fn publish_aggregated_sensor_data(
        &self,
        period: crate::libs::sensors::aggregation::AggregationPeriod,
        names: &[String; 8],
        locations: &[Option<String>; 8],
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

                // Get sensor name and location
                let name = &names[sensor.line as usize];
                let location = &locations[sensor.line as usize];

                json!({
                    "line": sensor.line,
                    "name": name,
                    "location": location,
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

    /// Publish system-level alarm event (power, wifi, ethernet)
    async fn publish_system_alarm_event(
        &self,
        alarm_type: &str,
        name: &str,
        from_state: &str,
        to_state: &str,
        message: &str,
    ) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "line": 0,
            "name": name,
            "from_state": from_state,
            "to_state": to_state,
            "temperature_celsius": null,
            "event_type": "system_alarm",
            "alarm_type": alarm_type,
            "message": message,
        });

        let topic = self.topics.alarms_events();
        let qos = Self::qos_from_u8(self.qos_overrides.alarm_events);

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish the device's standby state, plus a one-off event for the edge.
    ///
    /// The state topic is **retained**: a device in standby is off for as long as
    /// nobody reconnects PoE, which can be days, and a Viewer that subscribes in
    /// the meantime must still be able to tell "switched off by Dr Jane at 14:02"
    /// from "stopped answering". The non-retained event topic is what a
    /// subscriber already listening sees as it happens.
    async fn publish_standby_state(
        &self,
        standby: bool,
        reason: &str,
        requested_by: &str,
        entered_at: Option<&str>,
        vin_mv: u16,
    ) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "standby": standby,
            "reason": reason,
            "requested_by": requested_by,
            "entered_at": entered_at,
            "vin_mv": vin_mv,
        });
        let body = payload.to_string();
        let qos = Self::qos_from_u8(self.qos_overrides.alarm_events);

        // State first: if only one of the two makes it out, the durable one is
        // the one worth having.
        self.publish(self.topics.power_standby(), body.clone(), qos, true)
            .await?;
        self.publish(self.topics.power_events_standby(), body, qos, false)
            .await
    }

    /// Publish accelerometer motion transition event
    async fn publish_accelerometer_event(
        &self,
        x_g: f32,
        y_g: f32,
        z_g: f32,
        position: u8,
    ) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "event_type": "motion_transition",
            "x_g": x_g,
            "y_g": y_g,
            "z_g": z_g,
            "position": position,
        });

        let topic = self.topics.accelerometer_events();
        // Motion is high-frequency, non-critical telemetry — losing one
        // transition is harmless since the next event refreshes state.
        let qos = QoS::AtMostOnce;

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
        let last_dc_loss = last_dc_loss_time
            .and_then(|ts| DateTime::<Utc>::from_timestamp(ts as i64, 0).map(|dt| dt.to_rfc3339()));

        let firmware_version_str = if cfg!(feature = "dev-platform") {
            format!("{}-dev", version)
        } else {
            version.to_string()
        };

        let payload = json!({
            "timestamp": Self::timestamp(),
            "hostname": hostname,
            "device_label": device_label,
            "firmware_version": firmware_version_str,
            "dev_mode": cfg!(feature = "dev-platform"),
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
                // Both derived here rather than threaded through this function's
                // already-long positional signature: they are read from the local
                // journal and from /data, not from any caller's state.
                //
                // The unit's own radio EUI (system#7): registering a follower's
                // radio in a leader's ChirpStack needs it, and until now it was
                // only recoverable by shelling into the device. `null` on a unit
                // with no working concentrator.
                "gateway_eui": crate::libs::lorawan::cluster::own_gateway_eui(),
                // Cluster role, and for a follower its leader and the CA
                // fingerprint pinning it. Never the peer credential — this topic
                // is retained on the broker and read by every viewer.
                "cluster": crate::libs::lorawan::cluster::ClusterState::at_default().describe(),
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
            "dev_mode": cfg!(feature = "dev-platform"),
        });

        let topic = self.topics.status();
        let qos = QoS::AtLeastOnce;

        self.publish(topic, payload.to_string(), qos, true).await
    }

    /// Publish error message
    pub async fn publish_error(
        &self,
        command: &str,
        error: &str,
        message: &str,
    ) -> Result<(), String> {
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
            payload["applied_at_iso"] = json!(DateTime::<Utc>::from_timestamp(ts, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default());
        }

        // Add effective_at if present
        if let Some(ts) = effective_at {
            payload["effective_at"] = json!(ts);
            payload["effective_at_iso"] = json!(DateTime::<Utc>::from_timestamp(ts, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default());
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
                    "location": sensor.location,
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
        screen_timeout_secs: u32,
        buzzer_volume: u8,
        system_info_interval_s: u64,
        device_label: &str,
        sensors: Vec<super::messages::SensorConfigData>,
        lorawan_sensors: Vec<super::messages::LoRaWANSensorConfigData>,
        sample_interval_ms: u64,
        aggregation_interval_ms: u64,
        report_interval_ms: u64,
        beacon_enabled: bool,
        beacon_auto_provision: bool,
        beacon_auto_discover: bool,
    ) -> Result<(), String> {
        let sensors_data: Vec<serde_json::Value> = sensors
            .iter()
            .map(|sensor| {
                json!({
                    "line": sensor.line,
                    "name": sensor.name,
                    "location": sensor.location,
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

        let lorawan_sensors_data: Vec<serde_json::Value> = lorawan_sensors
            .iter()
            .map(|s| {
                json!({
                    "dev_eui": s.dev_eui,
                    "name": s.name,
                    "serial_number": s.serial_number,
                    "location": s.location,
                    "enabled": s.enabled,
                    "field_thresholds": s.field_thresholds,
                })
            })
            .collect();

        let payload = json!({
            "timestamp": Self::timestamp(),
            "led_brightness": led_brightness,
            "screen_brightness": screen_brightness,
            "screen_timeout_secs": screen_timeout_secs,
            "buzzer_volume": buzzer_volume,
            "system_info_interval_s": system_info_interval_s,
            "device_label": device_label,
            "sensors": sensors_data,
            "lorawan_sensors": lorawan_sensors_data,
            "intervals": {
                "sample_interval_ms": sample_interval_ms,
                "aggregation_interval_ms": aggregation_interval_ms,
                "report_interval_ms": report_interval_ms,
            },
            "eye": {
                "enabled": beacon_enabled,
                "auto_provision": beacon_auto_provision,
                "auto_discover": beacon_auto_discover,
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
        let payload = build_lorawan_sensors_payload(&sensors, &Self::timestamp());
        let topic = self.topics.lorawan_sensors();
        let qos = Self::qos_from_u8(self.qos_overrides.sensor_readings);
        self.publish(topic, payload, qos, false).await
    }

    /// Publish external LoRaWAN gateway status
    async fn publish_lorawan_gateways(
        &self,
        gateways: Vec<super::messages::LoRaWANGatewayPayload>,
    ) -> Result<(), String> {
        let gateways_data: Vec<serde_json::Value> = gateways
            .iter()
            .map(|g| {
                json!({
                    "gateway_eui": g.gateway_eui,
                    "name": g.name,
                    "online": g.online,
                    "last_seen": g.last_seen,
                })
            })
            .collect();

        let payload = json!({
            "timestamp": Self::timestamp(),
            "gateways": gateways_data,
        });

        let topic = self.topics.lorawan_gateways();
        let qos = Self::qos_from_u8(self.qos_overrides.sensor_readings);

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish EYE BLE tag sensor data
    async fn publish_beacon_sensors(
        &self,
        tags: Vec<super::messages::BeaconTagPayload>,
    ) -> Result<(), String> {
        let tags_data: Vec<serde_json::Value> = tags
            .iter()
            .map(|t| {
                json!({
                    "gateway": t.gateway,
                    "mac": t.mac,
                    "name": t.name,
                    "temperature_c": t.temperature_c,
                    "humidity_pct": t.humidity_pct,
                    "battery_mv": t.battery_mv,
                    "low_battery": t.low_battery,
                    "magnet_present": t.magnet_present,
                    "magnet_detected": t.magnet_detected,
                    "moving": t.moving,
                    "movement_count": t.movement_count,
                    "pitch_deg": t.pitch_deg,
                    "roll_deg": t.roll_deg,
                    "rssi": t.rssi,
                    "last_seen_ts": t.last_seen_ts,
                    "stale": t.stale,
                    "provisioning": t.provisioning,
                    "discovered": t.discovered,
                    "is_en12830": t.is_en12830,
                    "field_alarm_states": t.field_alarm_states,
                    "alarm_state": t.alarm_state,
                })
            })
            .collect();

        let payload = json!({
            "timestamp": Self::timestamp(),
            "tags": tags_data,
        });

        let topic = self.topics.beacon_sensors();
        let qos = Self::qos_from_u8(self.qos_overrides.sensor_readings);

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish a NODE's fPort-85 config read-back to
    /// `lorawan/sensors/<dev_eui>/config` (Feature C). `last_ack` lets the viewer
    /// render pending / awaiting-Ack / ok for a preceding write.
    async fn publish_node_config(
        &self,
        dev_eui: String,
        config: std::collections::BTreeMap<String, serde_json::Value>,
        page_index: u32,
        page_count: u32,
        last_seq: u32,
        last_result: String,
    ) -> Result<(), String> {
        let payload = json!({
            "dev_eui": dev_eui,
            "config": config,
            "page_index": page_index,
            "page_count": page_count,
            "synced_at": Self::timestamp(),
            "last_ack": { "seq": last_seq, "result": last_result },
        });

        let topic = self.topics.lorawan_sensor_config(&dev_eui);
        // Evict any retained value left on this topic by an older build before
        // publishing the real, non-retained snapshot. `/config` is deliberately
        // not retained — a config read is a point-in-time answer, and a retained
        // one is served to every new subscriber as if it were current. An earlier
        // build did retain it, and the leftover survives in the broker until
        // something overwrites it: measured on FIBER-CE3D59F8, a fresh subscriber
        // with no command in flight was handed a snapshot stamped four hours
        // earlier, which the viewer would render as the live config. The empty
        // retained payload is the standard tombstone, same as `clear_node_info`.
        let _ = self
            .publish(topic.clone(), String::new(), QoS::AtLeastOnce, true)
            .await;
        self.publish(topic, payload.to_string(), QoS::AtLeastOnce, false)
            .await
    }

    /// Publish the full non-secret config read-back. `chunks_done`/`chunks_total`
    /// are derived from `missing` rather than tracked separately: the reader
    /// batches at most `MAX_FIELDS_PER_GETPARAM` keys per chunk, so the counts a
    /// progress bar needs are just "how many keys landed out of how many asked".
    async fn publish_node_full_config(
        &self,
        dev_eui: String,
        config: std::collections::BTreeMap<String, serde_json::Value>,
        page_count: u32,
        last_seq: u32,
        read_status: String,
        missing: Vec<String>,
    ) -> Result<(), String> {
        let asked = config.len() + missing.len();
        let payload = json!({
            "dev_eui": dev_eui,
            "config": config,
            "page_count": page_count,
            "synced_at": Self::timestamp(),
            "last_seq": last_seq,
            "read_status": read_status,
            "missing": missing,
            "chunks_done": config.len(),
            "chunks_total": asked,
        });

        let topic = self.topics.lorawan_sensor_full_config(&dev_eui);
        self.publish(topic, payload.to_string(), QoS::AtLeastOnce, false)
            .await
    }

    /// Publish a NODE's fPort-85 device info to
    /// `lorawan/sensors/<dev_eui>/info` (#65), **retained**.
    ///
    /// Retained because this is device identity plus last-known health, and a
    /// node only reports every `interval_report` (900 s by default): a viewer
    /// that reconnects has to be able to render a firmware version and health
    /// flags immediately rather than showing blanks until someone re-queries.
    ///
    /// `info` arrives already projected by `node_config::info_to_json`, which
    /// reduces `claim_token` to `has_claim_token`. That redaction matters here
    /// specifically *because* the message is retained: the broker replays it to
    /// every future subscriber, so a secret published once would leak indefinitely.
    async fn publish_node_info(
        &self,
        dev_eui: String,
        info: serde_json::Value,
    ) -> Result<(), String> {
        let topic = self.topics.lorawan_sensor_info(&dev_eui);
        self.publish(topic, info.to_string(), QoS::AtLeastOnce, true)
            .await
    }

    /// Clear the retained device-info for a decommissioned node. Without this a
    /// removed device's info would be replayed to new subscribers forever.
    async fn clear_node_info(&self, dev_eui: &str) -> Result<(), String> {
        let topic = self.topics.lorawan_sensor_info(dev_eui);
        self.publish(topic, String::new(), QoS::AtLeastOnce, true)
            .await
    }

    /// Publish the outcome of a NODE control command (#71) to
    /// `lorawan/sensors/<dev_eui>/command`.
    ///
    /// `expect` is what makes this payload honest. Three of the five commands
    /// cannot report success at the moment they are acknowledged: `force_send`
    /// sends no fPort-85 reply at all, an empty-body `clock_sync` answers with a
    /// deferred `Info`, and `reboot`/`device_reset` restart 8 s later. Carrying the
    /// outstanding expectation lets the viewer render "requested" rather than
    /// claiming a success it cannot know about yet.
    #[allow(clippy::too_many_arguments)]
    async fn publish_node_command_result(
        &self,
        dev_eui: String,
        command: String,
        seq: u32,
        result: String,
        expect: Option<String>,
        detail: Option<String>,
        fault_key: Option<String>,
    ) -> Result<(), String> {
        let payload = json!({
            "dev_eui": dev_eui,
            "command": command,
            "seq": seq,
            "result": result,
            "expect": expect,
            "detail": detail,
            "fault_key": fault_key,
            "ts": Self::timestamp(),
        });
        let topic = self.topics.lorawan_sensor_command(&dev_eui);
        self.publish(topic, payload.to_string(), QoS::AtLeastOnce, false)
            .await
    }

    /// Publish one page of a NODE's on-device history to
    /// `lorawan/sensors/<dev_eui>/history` (Feature D).
    async fn publish_node_history(
        &self,
        dev_eui: String,
        frame_index: u32,
        frame_count: u32,
        records: Vec<serde_json::Value>,
    ) -> Result<(), String> {
        let payload = json!({
            "dev_eui": dev_eui,
            "frame_index": frame_index,
            "frame_count": frame_count,
            "records": records,
        });

        let topic = self.topics.lorawan_sensor_history(&dev_eui);
        self.publish(topic, payload.to_string(), QoS::AtLeastOnce, false)
            .await
    }

    /// Publish the result of a detect_eye_tag probe on `eye/detect`.
    async fn publish_beacon_detect_result(
        &self,
        mac: &str,
        is_en12830: Option<bool>,
        status: &str,
    ) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "mac": mac,
            "is_en12830": is_en12830,
            "status": status,
        });

        let topic = self.topics.beacon_detect();
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
        assert!(matches!(MqttPublisher::qos_from_u8(0), QoS::AtMostOnce));
        assert!(matches!(MqttPublisher::qos_from_u8(1), QoS::AtLeastOnce));
        assert!(matches!(MqttPublisher::qos_from_u8(2), QoS::ExactlyOnce));
    }

    fn sensor_with_backlog(
        dev_eui: &str,
        event_count: usize,
        gateway_count: usize,
    ) -> super::super::messages::LoRaWANSensorPayload {
        use crate::libs::lorawan::chirpstack::{GatewayRx, NodeEvent};

        let events = (0..event_count)
            .map(|i| NodeEvent {
                event_type: format!("event-{}", i),
                ts: format!("2026-08-14T02:{:02}:00Z", i % 60),
                // Realistic-sized nested payload, matching what a real alarm/
                // status event embeds via #[serde(flatten)] in production.
                extra: serde_json::json!({"detail": "x".repeat(200), "seq": i}),
            })
            .collect();

        let gateways = (0..gateway_count)
            .map(|i| GatewayRx {
                gateway_id: format!("gw-{:016}", i),
                rssi: Some(-40 - i as i32),
                snr: Some(9.5 - i as f32),
            })
            .collect();

        super::super::messages::LoRaWANSensorPayload {
            dev_eui: dev_eui.to_string(),
            name: "Test Node".to_string(),
            serial_number: Some("123456".to_string()),
            location: None,
            fields: std::collections::HashMap::from([("temperature".to_string(), 22.5)]),
            field_alarm_states: std::collections::HashMap::new(),
            field_thresholds: vec![],
            counters: std::collections::HashMap::new(),
            events,
            gateways,
            dr: Some(5),
            downlink_gateway_id: None,
            rssi: Some(-42),
            snr: Some(9.0),
            last_seen: Some("2026-08-14T02:18:39Z".to_string()),
            alarm_state: "OK".to_string(),
        }
    }

    #[test]
    fn lorawan_sensors_payload_stays_under_budget_with_a_full_event_backlog() {
        // 32 events matches MAX_RECENT_EVENTS; 10 gateways is a generous
        // real-world upper bound. This single sensor alone reproduced the
        // live ~14KB failure before the fix.
        let sensors = vec![sensor_with_backlog("587607079406ab88", 32, 10)];
        let payload = build_lorawan_sensors_payload(&sensors, "2026-08-14T02:20:00Z");
        assert!(
            payload.len() <= LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES,
            "payload was {} bytes, budget is {}",
            payload.len(),
            LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES
        );
    }

    #[test]
    fn lorawan_sensors_payload_caps_events_to_the_configured_limit() {
        let sensors = vec![sensor_with_backlog("587607079406ab88", 32, 1)];
        let payload = build_lorawan_sensors_payload(&sensors, "2026-08-14T02:20:00Z");
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let events = parsed["sensors"][0]["events"].as_array().unwrap();
        assert_eq!(events.len(), LORAWAN_SENSORS_EVENTS_CAP);
    }

    #[test]
    fn lorawan_sensors_payload_keeps_the_most_recent_events_when_capping() {
        let sensors = vec![sensor_with_backlog("587607079406ab88", 32, 1)];
        let payload = build_lorawan_sensors_payload(&sensors, "2026-08-14T02:20:00Z");
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let events = parsed["sensors"][0]["events"].as_array().unwrap();
        // Original events are named "event-0".."event-31"; capping to the
        // last N must keep "event-31" (most recent), not "event-0".
        let last = events.last().unwrap()["type"].as_str().unwrap();
        assert_eq!(last, format!("event-{}", 31));
    }

    #[test]
    fn lorawan_sensors_payload_keeps_essential_fields_even_when_capped() {
        let sensors = vec![sensor_with_backlog("587607079406ab88", 32, 10)];
        let payload = build_lorawan_sensors_payload(&sensors, "2026-08-14T02:20:00Z");
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let sensor = &parsed["sensors"][0];
        assert_eq!(sensor["dev_eui"], "587607079406ab88");
        assert_eq!(sensor["alarm_state"], "OK");
        assert_eq!(sensor["fields"]["temperature"], 22.5);
    }

    #[test]
    fn lorawan_sensors_payload_degrades_gracefully_with_many_sensors() {
        // 50 sensors each carrying a full backlog: even after per-sensor
        // capping this would exceed the budget, so the ladder must reach
        // its minimal-fields fallback, and the essential fields must still
        // all be present, and the payload must actually fit.
        let sensors: Vec<_> = (0..50)
            .map(|i| sensor_with_backlog(&format!("dev-eui-{:016}", i), 32, 10))
            .collect();
        let payload = build_lorawan_sensors_payload(&sensors, "2026-08-14T02:20:00Z");
        assert!(
            payload.len() <= LORAWAN_SENSORS_HARD_CEILING_BYTES,
            "payload was {} bytes, hard ceiling is {}",
            payload.len(),
            LORAWAN_SENSORS_HARD_CEILING_BYTES
        );
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["sensors"].as_array().unwrap().len(), 50);
        for sensor in parsed["sensors"].as_array().unwrap() {
            assert!(sensor["dev_eui"].as_str().unwrap().starts_with("dev-eui-"));
            assert_eq!(sensor["alarm_state"], "OK");
        }
    }

    /// Builds a sensor whose events are realistically-sized alarm reports
    /// (matching `node_payload.rs`'s ~186-byte serialized `NodeEvent` for a
    /// real fPort-3 AlarmReport), reproducing the exact live payload shape
    /// the final whole-branch review measured tripping the old two-tier
    /// fallback at 4 paired Nodes.
    fn sensor_with_realistic_alarm_backlog(
        dev_eui: &str,
    ) -> super::super::messages::LoRaWANSensorPayload {
        use crate::libs::lorawan::chirpstack::{GatewayRx, NodeEvent};

        let events = (0..8)
            .map(|i| NodeEvent {
                event_type: "alarm".to_string(),
                ts: format!("2026-08-14T02:{:02}:00Z", i % 60),
                extra: serde_json::json!({
                    "field": "temperature", "state": "CRITICAL_HIGH",
                    "value": 41.2, "threshold": 41.0, "line": 1,
                }),
            })
            .collect();
        let gateways = (0..5)
            .map(|i| GatewayRx {
                gateway_id: format!("gw-{:016}", i),
                rssi: Some(-40 - i as i32),
                snr: Some(9.5 - i as f32),
            })
            .collect();

        super::super::messages::LoRaWANSensorPayload {
            dev_eui: dev_eui.to_string(),
            name: "Test Node".to_string(),
            serial_number: Some("123456".to_string()),
            location: None,
            fields: std::collections::HashMap::from([
                ("temperature".to_string(), 41.2),
                ("humidity".to_string(), 55.0),
                ("battery".to_string(), 3.6),
            ]),
            field_alarm_states: std::collections::HashMap::from([(
                "temperature".to_string(),
                "CRITICAL_HIGH".to_string(),
            )]),
            field_thresholds: vec![],
            counters: std::collections::HashMap::new(),
            events,
            gateways,
            dr: Some(5),
            downlink_gateway_id: None,
            rssi: Some(-42),
            snr: Some(9.0),
            last_seen: Some("2026-08-14T02:18:39Z".to_string()),
            alarm_state: "CRITICAL".to_string(),
        }
    }

    #[test]
    fn lorawan_sensors_payload_keeps_alarm_events_for_a_realistic_multi_node_hub() {
        // 4 paired Nodes with realistic alarm-event sizes reproduces the
        // exact scenario the final review found tripping the old two-tier
        // fallback into events_cap=0 forever. The ladder must instead
        // degrade gateways first and still deliver at least one alarm event
        // per sensor.
        let sensors: Vec<_> = (0..4)
            .map(|i| sensor_with_realistic_alarm_backlog(&format!("dev-eui-{:016}", i)))
            .collect();
        let payload = build_lorawan_sensors_payload(&sensors, "2026-08-14T02:20:00Z");
        assert!(
            payload.len() <= LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES,
            "payload was {} bytes, budget is {}",
            payload.len(),
            LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES
        );
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        for sensor in parsed["sensors"].as_array().unwrap() {
            let events = sensor["events"].as_array().unwrap();
            assert!(
                !events.is_empty(),
                "expected at least one alarm event to survive capping for {:?}, got none",
                sensor["dev_eui"]
            );
        }
    }

    #[test]
    fn select_events_prefers_alarm_events_over_routine_ones_when_capping() {
        use crate::libs::lorawan::chirpstack::NodeEvent;

        // Oldest-first, as the real backlog is stored: 2 routine events,
        // then 2 alarm events, then 2 more routine events.
        let events: Vec<NodeEvent> = vec![
            ("boot", 0),
            ("orientation", 1),
            ("alarm", 2),
            ("alarm", 3),
            ("hall_active", 4),
            ("boot", 5),
        ]
        .into_iter()
        .map(|(t, i)| NodeEvent {
            event_type: t.to_string(),
            ts: format!("2026-08-14T02:{:02}:00Z", i),
            extra: serde_json::json!({"seq": i}),
        })
        .collect();

        // Cap of 2: without alarm-priority this would keep the 2 most
        // recent by position ("hall_active", "boot") and drop both alarms.
        let selected = select_events(&events, 2);
        assert_eq!(selected.len(), 2);
        assert!(
            selected.iter().all(|e| e.event_type == "alarm"),
            "expected both alarm events to be preferentially retained, got {:?}",
            selected.iter().map(|e| &e.event_type).collect::<Vec<_>>()
        );
    }
}
